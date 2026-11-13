# vasculho-dados

Índice de filiais por raiz de CNPJ, gerado uma vez por mês a partir dos
[dados abertos do CNPJ](https://arquivos.receitafederal.gov.br/) da Receita Federal.

Cada release `filiais-AAAA-MM` tem dois arquivos:

- `filiais-AAAA-MM.zip`: um banco SQLite (`filiais.db`) com a tabela
  `filial (raiz, ativas, total)`. `raiz` são os 8 primeiros caracteres do CNPJ;
  `ativas` e `total` contam os estabelecimentos marcados como filial, com situação
  cadastral ativa e em qualquer situação. Só entram raízes com ao menos uma filial.
- `latest.json`: mês da base, endereço, tamanho e SHA-256 do zip.

O índice tem só contagens por empresa. Não tem nome, endereço, sócio nem qualquer
dado pessoal.
