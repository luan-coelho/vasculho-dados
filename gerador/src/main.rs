//! Gera o índice de filiais por raiz de CNPJ a partir da base aberta da Receita.
//!
//! A base tem todos os estabelecimentos do país em vários zips (alguns GB por mês).
//! Este programa baixa um zip por vez do compartilhamento público da Receita, lê o
//! CSV em fluxo, conta as linhas de filial por raiz (os 8 primeiros caracteres do
//! CNPJ) e apaga o zip. No fim grava um SQLite pequeno, compacta e escreve o
//! `latest.json` com tamanho e SHA-256, prontos para virar release.
//!
//! O índice só tem contagens: raiz, filiais ativas e filiais no total. Só entram
//! raízes com ao menos uma filial. A raiz é texto: desde julho de 2026 o CNPJ pode
//! ter letras.

use std::collections::HashMap;
use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

const USER_AGENT: &str = "vasculho-dados/0.1 (indice de filiais por raiz de CNPJ)";

/// Versão do formato do índice. Quem lê recusa o que não conhece.
pub const SCHEMA: u32 = 1;

/// Compartilhamento público do CNPJ no Nextcloud da Receita. O código é o do link
/// público, não é segredo. A Receita já mudou de endereço uma vez: as variáveis
/// `VASCULHO_RECEITA_URL` e `VASCULHO_RECEITA_SHARE` trocam os dois sem recompilar.
const WEBDAV: &str = "https://arquivos.receitafederal.gov.br/public.php/webdav/";
const SHARE: &str = "YggdBLfdninEJX9";

fn webdav() -> String {
    let url = std::env::var("VASCULHO_RECEITA_URL").unwrap_or_else(|_| WEBDAV.to_string());
    if url.ends_with('/') { url } else { url + "/" }
}

/// Onde o índice é publicado.
const RELEASES: &str = "https://github.com/luan-coelho/vasculho-dados/releases";

/// Raiz do Banco do Brasil: milhares de agências, cada uma uma filial. Se a base
/// lida não mostrar isso, veio cortada ou o layout mudou.
const SANITY_ROOT: &str = "00000000";
const SANITY_MIN: u32 = 1000;

/// Filiais de uma raiz: as ativas na Receita e todas as que já existiram.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Branches {
    pub ativas: u32,
    pub total: u32,
}

const INDEX_DDL: &str = "
    CREATE TABLE filial (raiz TEXT PRIMARY KEY, ativas INTEGER NOT NULL, total INTEGER NOT NULL) WITHOUT ROWID;
    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;";

fn valid_month(month: &str) -> bool {
    let b = month.as_bytes();
    b.len() == 7
        && b[4] == b'-'
        && b.iter().enumerate().all(|(i, c)| i == 4 || c.is_ascii_digit())
        && (1..=12).contains(&month[5..].parse::<u32>().unwrap_or(0))
}

// ---------------------------------------------------------------------------
// Contagem a partir dos arquivos da Receita
// ---------------------------------------------------------------------------

/// Contagem de filiais por raiz, somada arquivo a arquivo.
#[derive(Debug, Default)]
pub struct Tally {
    roots: HashMap<[u8; 8], Branches>,
    /// Linhas lidas (matrizes e filiais).
    pub estabelecimentos: u64,
    /// Linhas de filial.
    pub filiais: u64,
    /// Linhas fora do formato, puladas.
    pub skipped: u64,
}

impl Tally {
    /// Soma um CSV de Estabelecimentos: `;`, aspas, sem cabeçalho. Só três colunas
    /// importam (raiz, matriz ou filial, situação) e são ASCII, então o resto, em
    /// Latin-1, nem é decodificado.
    pub fn read_csv(&mut self, reader: impl Read) -> Result<()> {
        let mut rows = csv::ReaderBuilder::new().delimiter(b';').has_headers(false).flexible(true).from_reader(reader);
        let mut row = csv::ByteRecord::new();
        while rows.read_byte_record(&mut row).context("lendo o arquivo da Receita")? {
            let (Some(root), Some(kind), Some(status)) = (row.get(0), row.get(3), row.get(5)) else {
                self.skipped += 1;
                continue;
            };
            let Some(root) = clean_root(root) else {
                self.skipped += 1;
                continue;
            };
            self.estabelecimentos += 1;
            // 1 é matriz, 2 é filial.
            if kind.trim_ascii() != b"2" {
                continue;
            }
            self.filiais += 1;
            let entry = self.roots.entry(root).or_default();
            entry.total += 1;
            // 02 é ativa.
            if status.trim_ascii() == b"02" {
                entry.ativas += 1;
            }
        }
        Ok(())
    }

    /// Soma um zip da Receita (um CSV dentro).
    pub fn read_zip(&mut self, path: &Path) -> Result<()> {
        let file = std::fs::File::open(path).with_context(|| format!("abrindo {}", path.display()))?;
        let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file))
            .with_context(|| format!("{} não é um zip válido", path.display()))?;
        for i in 0..zip.len() {
            let entry = zip.by_index(i)?;
            if entry.is_file() {
                self.read_csv(std::io::BufReader::with_capacity(1 << 20, entry))?;
            }
        }
        Ok(())
    }

    pub fn get(&self, root: &str) -> Option<Branches> {
        self.roots.get(&clean_root(root.as_bytes())?).copied()
    }

    /// Recusa uma base cortada ou num layout que mudou, antes de publicar. Com o mês
    /// anterior em mãos, o total de filiais não pode variar mais de 20%.
    pub fn validate(&self, previous: Option<&Manifest>) -> Result<()> {
        if self.roots.is_empty() {
            bail!("nenhuma filial lida: os arquivos vieram vazios ou o formato da Receita mudou");
        }
        let sanity = self.get(SANITY_ROOT).map_or(0, |b| b.ativas);
        if sanity < SANITY_MIN {
            bail!(
                "a base parece incompleta: o Banco do Brasil aparece com {sanity} filiais ativas (esperado: mais de {SANITY_MIN})"
            );
        }
        if let Some(prev) = previous.filter(|p| p.filiais > 0) {
            let (now, before) = (self.filiais as f64, prev.filiais as f64);
            if (now - before).abs() / before > 0.20 {
                bail!(
                    "o total de filiais mudou demais de um mês para o outro ({} em {}, {} agora); confira antes de publicar",
                    prev.filiais,
                    prev.mes,
                    self.filiais
                );
            }
        }
        Ok(())
    }

    /// Grava o índice SQLite em `path`.
    pub fn write_index(&self, path: &Path, month: &str) -> Result<()> {
        std::fs::remove_file(path).ok();
        let mut conn = Connection::open(path).with_context(|| format!("criando {}", path.display()))?;
        conn.execute_batch(INDEX_DDL)?;
        let mut roots: Vec<(&[u8; 8], &Branches)> = self.roots.iter().collect();
        roots.sort_unstable_by_key(|(root, _)| **root);
        let tx = conn.transaction()?;
        {
            let mut insert = tx.prepare("INSERT INTO filial (raiz, ativas, total) VALUES (?1, ?2, ?3)")?;
            for (root, b) in roots {
                insert.execute(params![std::str::from_utf8(root)?, b.ativas, b.total])?;
            }
            let mut meta = tx.prepare("INSERT INTO meta (key, value) VALUES (?1, ?2)")?;
            for (key, value) in [
                ("schema", SCHEMA.to_string()),
                ("mes", month.to_string()),
                ("estabelecimentos", self.estabelecimentos.to_string()),
                ("filiais", self.filiais.to_string()),
            ] {
                meta.execute(params![key, value])?;
            }
        }
        tx.commit()?;
        conn.execute_batch("VACUUM")?;
        Ok(())
    }
}

/// Raiz válida: 8 letras ou dígitos ASCII, em maiúsculas.
fn clean_root(raw: &[u8]) -> Option<[u8; 8]> {
    let raw = raw.trim_ascii();
    let mut root: [u8; 8] = raw.try_into().ok()?;
    if !root.iter().all(u8::is_ascii_alphanumeric) {
        return None;
    }
    root.make_ascii_uppercase();
    Some(root)
}

// ---------------------------------------------------------------------------
// Publicação
// ---------------------------------------------------------------------------

/// O `latest.json` do release: qual índice baixar e como conferir.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    /// `AAAA-MM` da base da Receita.
    pub mes: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub filiais: u64,
    pub estabelecimentos: u64,
}

/// Endereço do `latest.json`. `VASCULHO_FILIAIS_URL` troca, para testar outro release.
pub fn manifest_url() -> String {
    std::env::var("VASCULHO_FILIAIS_URL").unwrap_or_else(|_| format!("{RELEASES}/latest/download/latest.json"))
}

/// O `latest.json` já publicado, se houver: serve ao `--se-novo` e à validação
/// contra o mês anterior. Sem release ainda (ou sem rede), `None`.
fn published() -> Option<Manifest> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .http_status_as_error(false)
            .user_agent(USER_AGENT)
            .build(),
    );
    let mut response = agent.get(&manifest_url()).call().ok()?;
    if response.status().as_u16() != 200 {
        return None;
    }
    serde_json::from_str(&response.body_mut().read_to_string().ok()?).ok()
}

/// Empacota o índice de `tally` em `out`: `filiais-AAAA-MM.zip` e `latest.json`.
pub fn pack(tally: &Tally, month: &str, out: &Path) -> Result<Manifest> {
    if !valid_month(month) {
        bail!("mês inválido: {month} (use AAAA-MM)");
    }
    std::fs::create_dir_all(out).with_context(|| format!("criando {}", out.display()))?;
    let db = out.join("filiais.db");
    tally.write_index(&db, month)?;

    let name = format!("filiais-{month}.zip");
    let zip_path = out.join(&name);
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&zip_path)?);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).large_file(true);
    zip.start_file("filiais.db", options)?;
    std::io::copy(&mut std::fs::File::open(&db)?, &mut zip)?;
    zip.finish()?.flush()?;
    std::fs::remove_file(&db).ok();

    let (sha256, size) = sha256_of(&zip_path)?;
    let manifest = Manifest {
        schema: SCHEMA,
        mes: month.to_string(),
        url: format!("{RELEASES}/download/filiais-{month}/{name}"),
        sha256,
        size,
        filiais: tally.filiais,
        estabelecimentos: tally.estabelecimentos,
    };
    std::fs::write(out.join("latest.json"), serde_json::to_string_pretty(&manifest)? + "\n")?;
    Ok(manifest)
}

fn sha256_of(path: &Path) -> Result<(String, u64)> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((hex(&hasher.finalize()), size))
}

// ---------------------------------------------------------------------------
// Receita: listar e baixar pelo WebDAV
// ---------------------------------------------------------------------------

/// Um arquivo ou pasta do compartilhamento.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub size: u64,
    pub dir: bool,
}

/// Lê a resposta do PROPFIND. Não é um leitor de XML: procura as poucas marcas que
/// o Nextcloud devolve, com qualquer prefixo (`d:`, `D:`).
pub fn parse_propfind(xml: &str) -> Vec<Entry> {
    fn tag<'a>(block: &'a str, name: &str) -> Option<&'a str> {
        let start = block.find(&format!(":{name}>"))? + name.len() + 2;
        let rest = &block[start..];
        Some(rest[..rest.find('<')?].trim())
    }
    xml.split(":response>")
        .filter_map(|block| {
            let href = tag(block, "href")?;
            let name = tag(block, "displayname")
                .filter(|n| !n.is_empty())
                .or_else(|| href.trim_end_matches('/').rsplit('/').next())?
                .to_string();
            Some(Entry {
                name,
                size: tag(block, "getcontentlength").and_then(|s| s.parse().ok()).unwrap_or(0),
                dir: block.contains(":collection"),
            })
        })
        .collect()
}

fn basic_auth() -> String {
    let share = std::env::var("VASCULHO_RECEITA_SHARE").unwrap_or_else(|_| SHARE.to_string());
    format!("Basic {}", base64(format!("{share}:").as_bytes()))
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Lista uma pasta do compartilhamento (`""` é a raiz, com uma pasta por mês).
fn list(dir: &str) -> Result<Vec<Entry>> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(60)))
            .http_status_as_error(false)
            .allow_non_standard_methods(true)
            .user_agent(USER_AGENT)
            .build(),
    );
    let url = if dir.is_empty() { webdav() } else { format!("{}{dir}/", webdav()) };
    let body = r#"<?xml version="1.0"?><d:propfind xmlns:d="DAV:"><d:prop><d:displayname/><d:getcontentlength/><d:resourcetype/></d:prop></d:propfind>"#;
    let request = ureq::http::Request::builder()
        .method("PROPFIND")
        .uri(&url)
        .header("Authorization", basic_auth())
        .header("Depth", "1")
        .header("Content-Type", "text/xml")
        .body(body)?;
    let mut response = agent.run(request).context("a Receita não respondeu; tente mais tarde")?;
    let status = response.status().as_u16();
    if status != 207 {
        bail!("a Receita respondeu {status} ao listar {url}");
    }
    let xml = response.body_mut().read_to_string().context("lendo a lista de arquivos da Receita")?;
    // A própria pasta vem na lista; quem usa filtra por mês ou por arquivo.
    Ok(parse_propfind(&xml))
}

/// O mês mais novo entre as pastas `AAAA-MM`.
pub fn latest_month(entries: &[Entry]) -> Option<String> {
    entries.iter().filter(|e| e.dir && valid_month(&e.name)).map(|e| e.name.clone()).max()
}

/// Os zips de Estabelecimentos de uma lista, em ordem.
pub fn establishment_zips(entries: &[Entry]) -> Vec<Entry> {
    let mut zips: Vec<Entry> = entries
        .iter()
        .filter(|e| !e.dir && e.name.starts_with("Estabelecimentos") && e.name.to_ascii_lowercase().ends_with(".zip"))
        .cloned()
        .collect();
    zips.sort_by(|a, b| a.name.cmp(&b.name));
    zips
}

/// De onde vêm os zips de `build`.
pub enum Origin {
    /// Baixa da Receita; `None` é o mês mais novo publicado.
    Receita(Option<String>),
    /// Zips já baixados numa pasta, com o mês da base.
    Folder(PathBuf, String),
}

/// O que `build` fez.
pub enum Built {
    /// Índice gerado em `out`.
    Done(Manifest),
    /// `--se-novo`: o mês já está publicado.
    UpToDate(String),
}

/// Lê os Estabelecimentos e gera o índice em `out`. `only_if_new` sai sem baixar
/// nada quando o mês já está publicado. `log` recebe o andamento.
pub fn build(from: Origin, out: &Path, only_if_new: bool, mut log: impl FnMut(String)) -> Result<Built> {
    std::fs::create_dir_all(out).with_context(|| format!("criando {}", out.display()))?;
    let published = published();
    let mut tally = Tally::default();
    let month = match from {
        Origin::Folder(dir, month) => {
            let mut zips: Vec<PathBuf> = std::fs::read_dir(&dir)
                .with_context(|| format!("lendo {}", dir.display()))?
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
                    name.starts_with("Estabelecimentos") && name.to_ascii_lowercase().ends_with(".zip")
                })
                .collect();
            zips.sort();
            if zips.is_empty() {
                bail!("nenhum Estabelecimentos*.zip em {}", dir.display());
            }
            for zip in zips {
                log(format!("lendo {}", zip.display()));
                tally.read_zip(&zip)?;
            }
            month
        }
        Origin::Receita(month) => {
            let month = match month {
                Some(m) => m,
                None => latest_month(&list("")?).context("a Receita não tem nenhuma pasta de mês publicada")?,
            };
            if only_if_new && published.as_ref().is_some_and(|p| p.mes >= month) {
                return Ok(Built::UpToDate(month));
            }
            let zips = establishment_zips(&list(&month)?);
            if zips.is_empty() {
                bail!("a pasta {month} da Receita ainda não tem os arquivos de Estabelecimentos");
            }
            let total: u64 = zips.iter().map(|z| z.size).sum();
            log(format!("{} arquivos de {month}, {} MB", zips.len(), total / 1_000_000));
            let auth = basic_auth();
            for zip in &zips {
                let dest = out.join(format!("{}.part", zip.name));
                let url = format!("{}{month}/{}", webdav(), zip.name);
                if let Err(e) = fetch_zip(&url, &auth, &dest, zip.size, &mut log) {
                    std::fs::remove_file(&dest).ok();
                    return Err(e.context(format!("não deu para baixar {}", zip.name)));
                }
                let read = tally.read_zip(&dest);
                std::fs::remove_file(&dest).ok();
                read?;
                log(format!("  {} estabelecimentos lidos até aqui", tally.estabelecimentos));
            }
            month
        }
    };
    if only_if_new && published.as_ref().is_some_and(|p| p.mes >= month) {
        return Ok(Built::UpToDate(month));
    }
    tally.validate(published.as_ref().filter(|p| p.mes < month))?;
    Ok(Built::Done(pack(&tally, &month, out)?))
}

// ---------------------------------------------------------------------------
/// Velocidade mínima aceita para um download: abaixo disso, a tentativa desiste e
/// a próxima retoma de onde parou.
const MIN_SPEED: u64 = 200 * 1024;

/// Baixa um zip da Receita para `dest`. Os arquivos têm GB e a Receita derruba
/// conexões longas, então cada tentativa retoma de onde a anterior parou (`Range`)
/// em vez de recomeçar. `size` é o tamanho anunciado na lista; 0 é desconhecido.
fn fetch_zip(url: &str, auth: &str, dest: &Path, size: u64, log: &mut impl FnMut(String)) -> Result<()> {
    const ATTEMPTS: usize = 8;
    std::fs::remove_file(dest).ok();
    let name = url.rsplit('/').next().unwrap_or(url);
    let mut last = anyhow::anyhow!("nenhuma tentativa");
    for attempt in 1..=ATTEMPTS {
        let have = dest.metadata().map_or(0, |m| m.len());
        log(format!(
            "baixando {name} ({} MB), tentativa {attempt}{}",
            size / 1_000_000,
            if have > 0 { format!(", retomando de {} MB", have / 1_000_000) } else { String::new() }
        ));
        match fetch_part(url, auth, dest, have, size, log) {
            Ok(got) if size == 0 || got == size => return Ok(()),
            Ok(got) if got > size => {
                // Veio maior que o anunciado: o parcial não serve, recomeça do zero.
                std::fs::remove_file(dest).ok();
                last = anyhow::anyhow!("veio com {got} de {size} bytes");
            }
            Ok(got) => last = anyhow::anyhow!("a conexão fechou com {got} de {size} bytes"),
            Err(e) => {
                log(format!("  parou: {e:#}"));
                last = e;
            }
        }
    }
    Err(last)
}

/// Uma tentativa: pede a partir de `have` bytes e acrescenta ao que já está em
/// `dest`. Devolve o tamanho do arquivo no fim. Em erro, o parcial fica para a
/// próxima tentativa retomar.
fn fetch_part(url: &str, auth: &str, dest: &Path, have: u64, size: u64, log: &mut impl FnMut(String)) -> Result<u64> {
    // O ureq conta o tempo do corpo inteiro, não o tempo sem receber: o limite
    // acompanha o que falta baixar, com folga de 10 minutos.
    let budget = Duration::from_secs(600 + size.saturating_sub(have) / MIN_SPEED);
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(120)))
            .timeout_recv_body(Some(budget))
            .http_status_as_error(false)
            .user_agent(USER_AGENT)
            .build(),
    );
    let mut request = agent.get(url).header("Authorization", auth);
    if have > 0 {
        request = request.header("Range", format!("bytes={have}-"));
    }
    let response = request.call().context("a Receita não respondeu")?;
    let mut file = match response.status().as_u16() {
        206 if have > 0 => std::fs::OpenOptions::new().append(true).open(dest),
        // Sem suporte a retomada (ou primeira tentativa): começa do zero.
        200 => std::fs::File::create(dest),
        // Pediu além do fim: o arquivo já estava completo.
        416 if have > 0 => return Ok(have),
        status => bail!("a Receita respondeu {status}"),
    }
    .with_context(|| format!("gravando {}", dest.display()))?;
    let mut done = if response.status().as_u16() == 206 { have } else { 0 };
    let mut reader = response.into_body().into_with_config().limit(u64::MAX).reader();
    let mut buf = vec![0u8; 1024 * 1024];
    let step = (size / 10).max(1);
    let mut next = (done / step + 1) * step;
    let started = std::time::Instant::now();
    let from = done;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                file.flush().ok();
                return Err(anyhow::Error::new(e).context(format!("o download parou em {} MB", done / 1_000_000)));
            }
        };
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        done += n as u64;
        if size > 0 && done >= next {
            let secs = started.elapsed().as_secs_f64().max(0.001);
            log(format!(
                "  {}% ({} MB, {:.1} MB/s)",
                done * 100 / size,
                done / 1_000_000,
                (done - from) as f64 / 1_000_000.0 / secs
            ));
            next = (done / step + 1) * step;
        }
    }
    file.flush()?;
    Ok(done)
}

/// Bytes em hexadecimal minúsculo.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

const USAGE: &str = "\
uso: gerador [--mes AAAA-MM] [--de <pasta>] [--out <pasta>] [--se-novo]

  --mes AAAA-MM   mês da base (padrão: o mais novo publicado pela Receita)
  --de <pasta>    usa Estabelecimentos*.zip já baixados (aí --mes é obrigatório)
  --out <pasta>   onde gravar filiais-AAAA-MM.zip e latest.json (padrão ./dist)
  --se-novo       não faz nada se o mês já está publicado

VASCULHO_RECEITA_URL e VASCULHO_RECEITA_SHARE trocam o endereço e o código do
compartilhamento da Receita.";

fn main() -> Result<()> {
    let (mut month, mut folder, mut out, mut only_if_new) = (None, None, PathBuf::from("dist"), false);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mes" => month = Some(args.next().context("--mes precisa de AAAA-MM")?),
            "--de" => folder = Some(PathBuf::from(args.next().context("--de precisa de uma pasta")?)),
            "--out" => out = PathBuf::from(args.next().context("--out precisa de uma pasta")?),
            "--se-novo" => only_if_new = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => bail!("opção desconhecida: {other}\n\n{USAGE}"),
        }
    }
    let from = match folder {
        Some(dir) => Origin::Folder(dir, month.context("com --de, diga o mês da base em --mes AAAA-MM")?),
        None => Origin::Receita(month),
    };
    match build(from, &out, only_if_new, |line| eprintln!("{line}"))? {
        Built::UpToDate(m) => println!("nada novo: {m} já está publicado"),
        Built::Done(m) => println!(
            "índice de {} em {}: {} filiais de {} estabelecimentos, {} KB",
            m.mes,
            out.display(),
            m.filiais,
            m.estabelecimentos,
            m.size / 1000
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Linha de Estabelecimentos com as colunas que importam; o nome fantasia leva
    /// um byte Latin-1 (0xC7, "Ç") para provar que não atrapalha.
    fn row(root: &str, order: &str, kind: &str, status: &str) -> Vec<u8> {
        let mut line = format!("\"{root}\";\"{order}\";\"00\";\"{kind}\";\"A").into_bytes();
        line.push(0xC7);
        line.extend(format!("AI\";\"{status}\";\"20200101\"\n").into_bytes());
        line
    }

    fn sample() -> Vec<u8> {
        [
            row("11111111", "0001", "1", "02"),
            row("11111111", "0002", "2", "02"),
            row("11111111", "0003", "2", "08"),
            row("11111111", "0004", "2", "02"),
            row("22222222", "0001", "1", "02"), // só matriz
            row("12abc345", "0001", "1", "02"), // alfanumérico, minúsculas
            row("12ABC345", "0002", "2", "04"),
            b"\"curta\";\"x\"\n".to_vec(),
        ]
        .concat()
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vasculho-filiais-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn zip_with(path: &Path, inner: &str, bytes: &[u8]) {
        let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        zip.start_file(inner, zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(bytes).unwrap();
        zip.finish().unwrap();
    }

    #[test]
    fn counts_branches_per_root() {
        let mut tally = Tally::default();
        tally.read_csv(sample().as_slice()).unwrap();
        assert_eq!(tally.get("11111111"), Some(Branches { ativas: 2, total: 3 }));
        assert_eq!(tally.get("22222222"), None, "só matriz fica fora do índice");
        assert_eq!(tally.get("12abc345"), Some(Branches { ativas: 0, total: 1 }), "raiz com letra, em maiúsculas");
        assert_eq!((tally.estabelecimentos, tally.filiais, tally.skipped), (7, 4, 1));
    }

    #[test]
    fn reads_the_receita_zip_and_writes_the_index() {
        let dir = temp("zip");
        let zip = dir.join("Estabelecimentos0.zip");
        zip_with(&zip, "K3241.K03200Y0.D51011.ESTABELE", &sample());
        let mut tally = Tally::default();
        tally.read_zip(&zip).unwrap();

        let manifest = pack(&tally, "2026-10", &dir).unwrap();
        assert_eq!((manifest.mes.as_str(), manifest.filiais, manifest.schema), ("2026-10", 4, SCHEMA));
        assert!(manifest.url.ends_with("/download/filiais-2026-10/filiais-2026-10.zip"));
        let (sha256, size) = sha256_of(&dir.join("filiais-2026-10.zip")).unwrap();
        assert_eq!((sha256, size), (manifest.sha256.clone(), manifest.size));
        let saved: Manifest = serde_json::from_slice(&std::fs::read(dir.join("latest.json")).unwrap()).unwrap();
        assert_eq!(saved, manifest);
        assert!(pack(&tally, "2026-13", &dir).is_err(), "mês inválido");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn validate_refuses_a_broken_base() {
        assert!(Tally::default().validate(None).is_err(), "vazia");
        let mut tally = Tally::default();
        tally.read_csv(sample().as_slice()).unwrap();
        assert!(tally.validate(None).is_err(), "sem as agências do Banco do Brasil a base está cortada");

        let mut bb = Vec::new();
        for i in 0..1200 {
            bb.extend(row(SANITY_ROOT, &format!("{:04}", i + 2), "2", "02"));
        }
        tally.read_csv(bb.as_slice()).unwrap();
        tally.validate(None).unwrap();
        let prev = |filiais| Manifest {
            schema: SCHEMA,
            mes: "2026-09".into(),
            url: String::new(),
            sha256: String::new(),
            size: 0,
            filiais,
            estabelecimentos: 0,
        };
        tally.validate(Some(&prev(1100))).unwrap();
        assert!(tally.validate(Some(&prev(3000))).is_err(), "caiu mais de 20%");
    }

    #[test]
    fn parses_the_webdav_listing() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response><d:href>/public.php/webdav/</d:href><d:propstat><d:prop><d:displayname></d:displayname><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
  <d:response><d:href>/public.php/webdav/2026-09/</d:href><d:propstat><d:prop><d:displayname>2026-09</d:displayname><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
  <d:response><d:href>/public.php/webdav/2026-10/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
  <d:response><d:href>/public.php/webdav/2026-10/Estabelecimentos1.zip</d:href><d:propstat><d:prop><d:displayname>Estabelecimentos1.zip</d:displayname><d:getcontentlength>488344875</d:getcontentlength><d:resourcetype/></d:prop></d:propstat></d:response>
  <d:response><d:href>/public.php/webdav/2026-10/Estabelecimentos0.zip</d:href><d:propstat><d:prop><d:displayname>Estabelecimentos0.zip</d:displayname><d:getcontentlength>12</d:getcontentlength><d:resourcetype/></d:prop></d:propstat></d:response>
  <d:response><d:href>/public.php/webdav/2026-10/Empresas0.zip</d:href><d:propstat><d:prop><d:displayname>Empresas0.zip</d:displayname><d:getcontentlength>5</d:getcontentlength><d:resourcetype/></d:prop></d:propstat></d:response>
</d:multistatus>"#;
        let entries = parse_propfind(xml);
        assert_eq!(entries.len(), 6);
        assert_eq!(latest_month(&entries).as_deref(), Some("2026-10"), "sem displayname, vale o fim do href");
        let zips = establishment_zips(&entries);
        assert_eq!(
            zips.iter().map(|z| (z.name.as_str(), z.size)).collect::<Vec<_>>(),
            [("Estabelecimentos0.zip", 12), ("Estabelecimentos1.zip", 488_344_875)]
        );
        assert_eq!(base64(b"YggdBLfdninEJX9:"), "WWdnZEJMZmRuaW5FSlg5Og==");
        assert_eq!(base64(b"ab"), "YWI=");
    }
}
