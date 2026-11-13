# vasculho-dados

Índice de filiais por raiz de CNPJ, gerado uma vez por mês a partir dos
[dados abertos do CNPJ](https://arquivos.receitafederal.gov.br/) da Receita Federal.

O índice tem só contagens por empresa. Não tem nome, endereço, sócio nem qualquer
dado pessoal.

## O que cada release tem

Cada release `filiais-AAAA-MM` tem dois arquivos:

- `filiais-AAAA-MM.zip`: um banco SQLite (`filiais.db`) com a tabela
  `filial (raiz, ativas, total)`. `raiz` são os 8 primeiros caracteres do CNPJ;
  `ativas` e `total` contam os estabelecimentos marcados como filial, com situação
  cadastral ativa e em qualquer situação. Só entram raízes com ao menos uma filial:
  raiz fora da tabela é empresa sem filial. A tabela `meta` guarda o mês da base e
  os totais.
- `latest.json`: mês da base, endereço, tamanho e SHA-256 do zip. O endereço
  `releases/latest/download/latest.json` aponta sempre para o mais novo.

## Como é gerado

O programa em `gerador/` baixa os arquivos de Estabelecimentos da Receita um por
vez, lê cada CSV em fluxo, conta as linhas de filial por raiz e apaga o arquivo.
Antes de gravar, recusa uma base cortada ou com o total de filiais muito diferente
do mês anterior.

O workflow `filiais.yml` roda toda segunda-feira e só faz algo quando a Receita
publicou um mês novo.

Para rodar à mão:

```
cd gerador
cargo run --release -- --out dist                   # mês mais novo, baixando da Receita
cargo run --release -- --de <pasta> --mes AAAA-MM   # com zips já baixados
```

A Receita já mudou os arquivos de endereço uma vez. `VASCULHO_RECEITA_URL` e
`VASCULHO_RECEITA_SHARE` trocam o endereço e o código do compartilhamento sem
recompilar.
