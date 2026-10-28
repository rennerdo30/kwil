# Bundled grammar provenance

Verified 2026-10-23. Kwil is MIT; this bundle uses only MIT and Apache-2.0
components. There are 32 selected grammars, plus the transitive JSDoc grammar.
Kotlin's highlight query has a separate Apache-2.0 notice. Full upstream licence
texts, including copyright notices, are retained in [LICENSES.txt](LICENSES.txt).

## Choice for Rust

Replaced arborium-rust 2.18.2 (the unavailable orchard fork) with the official
[tree-sitter-rust 0.24.2](https://crates.io/crates/tree-sitter-rust/0.24.2), latest
non-yanked release checked via the crates.io API on 2026-10-23 (released
2026-03-27). The existing lang-rust feature stays independent; the upstream
LanguageFn works with tree-sitter 0.27.1. No runtime dependency was added.

The crate's .cargo_vcs_info.json names e2bee853694a1d3e0f6ef308fe3674542fec95d7,
which was unavailable upstream. Instead of trusting that field, verified the
published v0.24.2 tag at 77a3747266f4d621d0757825e6b11edcbf991ca5. The packaged
LICENSE, grammar.js, queries/highlights.scm, src/parser.c and src/scanner.c each
byte-match that pinned official source. Their SHA-256 values are:

| File | SHA-256 |
|---|---|
| LICENSE | 31d5b6f4243d5c7c6e1c4ebbbb9f6407bd1457a08bcc4f706521710341acba36 |
| grammar.js | e2b851418e14e3bacec9ef5993e0f8f05196a76641a5e1d488ab86d6973b3dc4 |
| queries/highlights.scm | 0f0343107f14a7690157f51090a979eb8f8bfe4eada7c61763ddb4c54b1311d1 |
| src/parser.c | 9602518f9e57919910bf0e777e52f6bfc9325d4c182e998bdb4efd5682b76e4a |
| src/scanner.c | 9609a2f92dbb7c32bc056fd8fb94e5478428f04496696aa08b048a9b66caf283 |

## Verification method

Every crate tarball SHA-256 below matches Cargo.lock. For Arborium, read the
packaged Cargo.toml.orig licence declaration and README upstream repository and
commit, then fetched and read the licence at that exact upstream commit. These
pins identify upstream provenance; Arborium may regenerate or patch generated
parsers, so they are not claims that every Arborium C file byte-matches upstream.
The tarball hashes identify the exact shipped source, queries and wrappers.

Ruby's published README leaves its upstream commit blank. Its exact source is
identified by the arborium-ruby 2.18.2 tarball hash. The Ruby commit below is an
independently fetched upstream MIT licence snapshot, **not a claimed parser
commit**. The shipped manifest and README both declare MIT; the upstream source
repository remains available. Unlike the excluded orchard fork, this does not
depend on an unavailable source repository.

Kotlin's packaged queries/highlights.scm header points to nvim-treesitter at the
listed commit and says its Lua-match predicates were removed. Fetched and read
that project's Apache-2.0 licence separately. Other selected query files contain
no separate licence notice. Inherited JavaScript/JSDoc queries are included in
this inventory. No GPL grammar is enabled. WASM-only arborium-sysroot is not a
grammar and is outside this grammar inventory.

Reproduce the network and cached-source audit from the repo root with
`& crates/core-syntax/tools/audit-licenses.ps1`. It checks checksums and Rust
source equality and writes a JSON record to the external cache. Network calls
have per-request timeouts. A is the Arborium README/manifest/upstream check;
R adds official Rust source equality; B is Ruby's explicit blank-pin case;
K is the separate Kotlin query notice.

## Source inventory

| Grammar / component | Crate version | Upstream source and commit | Licence text | Verified |
|---|---|---|---|---|
| arborium-c | 2.18.2 | [ae19b676b13bdcc13b7665397e6d9b14975473dd](https://github.com/tree-sitter/tree-sitter-c/tree/ae19b676b13bdcc13b7665397e6d9b14975473dd) | [MIT](https://raw.githubusercontent.com/tree-sitter/tree-sitter-c/ae19b676b13bdcc13b7665397e6d9b14975473dd/LICENSE) | A |
| arborium-cpp | 2.18.2 | [12bd6f7e96080d2e70ec51d4068f2f66120dde35](https://github.com/tree-sitter/tree-sitter-cpp/tree/12bd6f7e96080d2e70ec51d4068f2f66120dde35) | [MIT](https://raw.githubusercontent.com/tree-sitter/tree-sitter-cpp/12bd6f7e96080d2e70ec51d4068f2f66120dde35/LICENSE) | A |
| arborium-c-sharp | 2.18.2 | [485f0bae0274ac9114797fc10db6f7034e4086e3](https://github.com/tree-sitter/tree-sitter-c-sharp/tree/485f0bae0274ac9114797fc10db6f7034e4086e3) | [MIT](https://raw.githubusercontent.com/tree-sitter/tree-sitter-c-sharp/485f0bae0274ac9114797fc10db6f7034e4086e3/LICENSE) | A |
| arborium-css | 2.18.2 | [dda5cfc5722c429eaba1c910ca32c2c0c5bb1a3f](https://github.com/tree-sitter/tree-sitter-css/tree/dda5cfc5722c429eaba1c910ca32c2c0c5bb1a3f) | [MIT](https://raw.githubusercontent.com/tree-sitter/tree-sitter-css/dda5cfc5722c429eaba1c910ca32c2c0c5bb1a3f/LICENSE) | A |
| arborium-go | 2.18.2 | [2346a3ab1bb3857b48b29d779a1ef9799a248cd7](https://github.com/tree-sitter/tree-sitter-go/tree/2346a3ab1bb3857b48b29d779a1ef9799a248cd7) | [MIT](https://raw.githubusercontent.com/tree-sitter/tree-sitter-go/2346a3ab1bb3857b48b29d779a1ef9799a248cd7/LICENSE) | A |
| arborium-html | 2.18.2 | [73a3947324f6efddf9e17c0ea58d454843590cc0](https://github.com/tree-sitter/tree-sitter-html/tree/73a3947324f6efddf9e17c0ea58d454843590cc0) | [MIT](https://raw.githubusercontent.com/tree-sitter/tree-sitter-html/73a3947324f6efddf9e17c0ea58d454843590cc0/LICENSE) | A |
| arborium-java | 2.18.2 | [e10607b45ff745f5f876bfa3e94fbcc6b44bdc11](https://github.com/tree-sitter/tree-sitter-java/tree/e10607b45ff745f5f876bfa3e94fbcc6b44bdc11) | [MIT](https://raw.githubusercontent.com/tree-sitter/tree-sitter-java/e10607b45ff745f5f876bfa3e94fbcc6b44bdc11/LICENSE) | A |
| arborium-javascript | 2.18.2 | [58404d8cf191d69f2674a8fd507bd5776f46cb11](https://github.com/tree-sitter/tree-sitter-javascript/tree/58404d8cf191d69f2674a8fd507bd5776f46cb11) | [MIT](https://raw.githubusercontent.com/tree-sitter/tree-sitter-javascript/58404d8cf191d69f2674a8fd507bd5776f46cb11/LICENSE) | A |
| arborium-json | 2.18.2 | [001c28d7a29832b06b0e831ec77845553c89b56d](https://github.com/tree-sitter/tree-sitter-json/tree/001c28d7a29832b06b0e831ec77845553c89b56d) | [MIT](https://raw.githubusercontent.com/tree-sitter/tree-sitter-json/001c28d7a29832b06b0e831ec77845553c89b56d/LICENSE) | A |
| arborium-kotlin | 2.18.2 | [57fb4560ba8641865bc0baa6b3f413b236112c4c](https://github.com/fwcd/tree-sitter-kotlin/tree/57fb4560ba8641865bc0baa6b3f413b236112c4c) | [MIT](https://raw.githubusercontent.com/fwcd/tree-sitter-kotlin/57fb4560ba8641865bc0baa6b3f413b236112c4c/LICENSE) | A |
| arborium-lua | 2.18.2 | [d76023017f7485eae629cb60d406c7a1ca0f40c9](https://github.com/tree-sitter-grammars/tree-sitter-lua/tree/d76023017f7485eae629cb60d406c7a1ca0f40c9) | [MIT](https://raw.githubusercontent.com/tree-sitter-grammars/tree-sitter-lua/d76023017f7485eae629cb60d406c7a1ca0f40c9/LICENSE.md) | A |
| arborium-markdown | 2.18.2 | [2dfd57f547f06ca5631a80f601e129d73fc8e9f0](https://github.com/tree-sitter-grammars/tree-sitter-markdown/tree/2dfd57f547f06ca5631a80f601e129d73fc8e9f0) | [MIT](https://raw.githubusercontent.com/tree-sitter-grammars/tree-sitter-markdown/2dfd57f547f06ca5631a80f601e129d73fc8e9f0/LICENSE) | A |
| arborium-php | 2.18.2 | [7d07b41ce2d442ca9a90ed85d0075eccc17ae315](https://github.com/tree-sitter/tree-sitter-php/tree/7d07b41ce2d442ca9a90ed85d0075eccc17ae315) | [MIT](https://raw.githubusercontent.com/tree-sitter/tree-sitter-php/7d07b41ce2d442ca9a90ed85d0075eccc17ae315/LICENSE) | A |
| arborium-python | 2.18.2 | [26855eabccb19c6abf499fbc5b8dc7cc9ab8bc64](https://github.com/tree-sitter/tree-sitter-python/tree/26855eabccb19c6abf499fbc5b8dc7cc9ab8bc64) | [MIT](https://raw.githubusercontent.com/tree-sitter/tree-sitter-python/26855eabccb19c6abf499fbc5b8dc7cc9ab8bc64/LICENSE) | A |
| arborium-ruby | 2.18.2 | [ad907a69da0c8a4f7a943a7fe012712208da6dee](https://github.com/tree-sitter/tree-sitter-ruby/tree/ad907a69da0c8a4f7a943a7fe012712208da6dee) | [MIT](https://raw.githubusercontent.com/tree-sitter/tree-sitter-ruby/ad907a69da0c8a4f7a943a7fe012712208da6dee/LICENSE) | B |
| arborium-bash | 2.18.2 | [20f55fb9663568f16ebceaac7673d2aa530add46](https://github.com/tree-sitter/tree-sitter-bash/tree/20f55fb9663568f16ebceaac7673d2aa530add46) | [MIT](https://raw.githubusercontent.com/tree-sitter/tree-sitter-bash/20f55fb9663568f16ebceaac7673d2aa530add46/LICENSE) | A |
| arborium-sql | 2.18.2 | [fe77f6868d6cdea593052a6af390116495093dc1](https://github.com/DerekStride/tree-sitter-sql/tree/fe77f6868d6cdea593052a6af390116495093dc1) | [MIT](https://raw.githubusercontent.com/DerekStride/tree-sitter-sql/fe77f6868d6cdea593052a6af390116495093dc1/LICENSE) | A |
| arborium-swift | 2.18.2 | [78d84ef82c387fceeb6094038da28717ea052e39](https://github.com/alex-pinkus/tree-sitter-swift/tree/78d84ef82c387fceeb6094038da28717ea052e39) | [MIT](https://raw.githubusercontent.com/alex-pinkus/tree-sitter-swift/78d84ef82c387fceeb6094038da28717ea052e39/LICENSE) | A |
| arborium-toml | 2.18.2 | [64b56832c2cffe41758f28e05c756a3a98d16f41](https://github.com/tree-sitter-grammars/tree-sitter-toml/tree/64b56832c2cffe41758f28e05c756a3a98d16f41) | [MIT](https://raw.githubusercontent.com/tree-sitter-grammars/tree-sitter-toml/64b56832c2cffe41758f28e05c756a3a98d16f41/LICENSE) | A |
| arborium-typescript | 2.18.2 | [75b3874edb2dc714fb1fd77a32013d0f8699989f](https://github.com/tree-sitter/tree-sitter-typescript/tree/75b3874edb2dc714fb1fd77a32013d0f8699989f) | [MIT](https://raw.githubusercontent.com/tree-sitter/tree-sitter-typescript/75b3874edb2dc714fb1fd77a32013d0f8699989f/LICENSE) | A |
| arborium-tsx | 2.18.2 | [75b3874edb2dc714fb1fd77a32013d0f8699989f](https://github.com/tree-sitter/tree-sitter-typescript/tree/75b3874edb2dc714fb1fd77a32013d0f8699989f) | [MIT](https://raw.githubusercontent.com/tree-sitter/tree-sitter-typescript/75b3874edb2dc714fb1fd77a32013d0f8699989f/LICENSE) | A |
| arborium-xml | 2.18.2 | [5000ae8f22d11fbe93939b05c1e37cf21117162d](https://github.com/tree-sitter-grammars/tree-sitter-xml/tree/5000ae8f22d11fbe93939b05c1e37cf21117162d) | [MIT](https://raw.githubusercontent.com/tree-sitter-grammars/tree-sitter-xml/5000ae8f22d11fbe93939b05c1e37cf21117162d/LICENSE) | A |
| arborium-yaml | 2.18.2 | [7708026449bed86239b1cd5bce6e3c34dbca6415](https://github.com/tree-sitter-grammars/tree-sitter-yaml/tree/7708026449bed86239b1cd5bce6e3c34dbca6415) | [MIT](https://raw.githubusercontent.com/tree-sitter-grammars/tree-sitter-yaml/7708026449bed86239b1cd5bce6e3c34dbca6415/LICENSE) | A |
| arborium-dockerfile | 2.18.2 | [971acdd908568b4531b0ba28a445bf0bb720aba5](https://github.com/camdencheek/tree-sitter-dockerfile/tree/971acdd908568b4531b0ba28a445bf0bb720aba5) | [MIT](https://raw.githubusercontent.com/camdencheek/tree-sitter-dockerfile/971acdd908568b4531b0ba28a445bf0bb720aba5/LICENSE) | A |
| arborium-make | 2.18.2 | [70613f3d812cbabbd7f38d104d60a409c4008b43](https://github.com/tree-sitter-grammars/tree-sitter-make/tree/70613f3d812cbabbd7f38d104d60a409c4008b43) | [MIT](https://raw.githubusercontent.com/tree-sitter-grammars/tree-sitter-make/70613f3d812cbabbd7f38d104d60a409c4008b43/LICENSE) | A |
| arborium-ini | 2.18.2 | [bea223734aa9cea2c8793bf9098bd7fdef912f57](https://github.com/justinmk/tree-sitter-ini/tree/bea223734aa9cea2c8793bf9098bd7fdef912f57) | [Apache-2.0](https://raw.githubusercontent.com/justinmk/tree-sitter-ini/bea223734aa9cea2c8793bf9098bd7fdef912f57/LICENSE) | A |
| arborium-diff | 2.18.2 | [2520c3f934b3179bb540d23e0ef45f75304b5fed](https://github.com/the-mikedavis/tree-sitter-diff/tree/2520c3f934b3179bb540d23e0ef45f75304b5fed) | [MIT](https://raw.githubusercontent.com/the-mikedavis/tree-sitter-diff/2520c3f934b3179bb540d23e0ef45f75304b5fed/LICENSE) | A |
| arborium-batch | 2.18.2 | [737a031b42240bf61bf7ea5e4356d4e0580dd6d9](https://github.com/davidevofficial/tree-sitter-batch/tree/737a031b42240bf61bf7ea5e4356d4e0580dd6d9) | [MIT](https://raw.githubusercontent.com/davidevofficial/tree-sitter-batch/737a031b42240bf61bf7ea5e4356d4e0580dd6d9/LICENSE) | A |
| arborium-powershell | 2.18.2 | [9379c77984af1f3d3d7e3cc5e897de3496725280](https://github.com/airbus-cert/tree-sitter-powershell/tree/9379c77984af1f3d3d7e3cc5e897de3496725280) | [MIT](https://raw.githubusercontent.com/airbus-cert/tree-sitter-powershell/9379c77984af1f3d3d7e3cc5e897de3496725280/LICENSE) | A |
| arborium-haskell | 2.18.2 | [98aedbd2d6947a168ba3ba3755d70b0cb6b78395](https://github.com/tree-sitter-grammars/tree-sitter-haskell/tree/98aedbd2d6947a168ba3ba3755d70b0cb6b78395) | [MIT](https://raw.githubusercontent.com/tree-sitter-grammars/tree-sitter-haskell/98aedbd2d6947a168ba3ba3755d70b0cb6b78395/LICENSE) | A |
| arborium-scala | 2.18.2 | [97aead18d97708190a51d4f551ea9b05b60641c9](https://github.com/tree-sitter/tree-sitter-scala/tree/97aead18d97708190a51d4f551ea9b05b60641c9) | [MIT](https://raw.githubusercontent.com/tree-sitter/tree-sitter-scala/97aead18d97708190a51d4f551ea9b05b60641c9/LICENSE) | A |
| arborium-jsdoc | 2.18.2 | [658d18dcdddb75c760363faa4963427a7c6b52db](https://github.com/tree-sitter/tree-sitter-jsdoc/tree/658d18dcdddb75c760363faa4963427a7c6b52db) | [MIT](https://raw.githubusercontent.com/tree-sitter/tree-sitter-jsdoc/658d18dcdddb75c760363faa4963427a7c6b52db/LICENSE) | A |
| tree-sitter-rust | 0.24.2 | [77a3747266f4d621d0757825e6b11edcbf991ca5](https://github.com/tree-sitter/tree-sitter-rust/tree/77a3747266f4d621d0757825e6b11edcbf991ca5) | [MIT](https://raw.githubusercontent.com/tree-sitter/tree-sitter-rust/77a3747266f4d621d0757825e6b11edcbf991ca5/LICENSE) | R |
| kotlin-highlight-query | in arborium-kotlin 2.18.2 | [f8ab59861eed4a1c168505e3433462ed800f2bae](https://github.com/nvim-treesitter/nvim-treesitter/tree/f8ab59861eed4a1c168505e3433462ed800f2bae) | [Apache-2.0](https://raw.githubusercontent.com/nvim-treesitter/nvim-treesitter/f8ab59861eed4a1c168505e3433462ed800f2bae/LICENSE) | K |

## Artifact checksums

SHA-256 of the .crate archive and fetched UTF-8 licence bytes. Kotlin query uses its containing crate checksum.

| Component | Crate SHA-256 | Licence SHA-256 |
|---|---|---|
| arborium-c | 37615879a3956f8918d43be52c9c4d11d60af5b7bedabca87ab8b47f769fa8c7 | 2e0110e07abef7c2548b26ec9d6969775617ca539a0dc8dbeeb14d6452c711d1 |
| arborium-cpp | 736c482444d8d58e7f3cd8df7bc88bf6c4b450e9039a55c43b3aec39d6a805ce | 2e0110e07abef7c2548b26ec9d6969775617ca539a0dc8dbeeb14d6452c711d1 |
| arborium-c-sharp | 1da8f9108bf98f334190243c27080476b126c54981fe2cb9b04d4f25d092960e | 778fb7d63b8c1844da315648c02f325c4713a6f4a5d19644fd413421422776d3 |
| arborium-css | d3bf02fc773bafaac2cdc27862d6eb52e86927e740c69e0d87b4123f2589b9af | c5cfb43042b6b72045f4ba997834d0a7786d2793d91680868b5815b39f14fc78 |
| arborium-go | f297634bca52a796de2725149dc684c18286c78f0a096fa27d532f9627bb1e02 | 2e0110e07abef7c2548b26ec9d6969775617ca539a0dc8dbeeb14d6452c711d1 |
| arborium-html | 6ed785a76d53a63ff144b26a68b789fe4d10169a5f683d573ee72f3234d8a9a4 | 2e0110e07abef7c2548b26ec9d6969775617ca539a0dc8dbeeb14d6452c711d1 |
| arborium-java | ad9a5d6a278548b7a18bbaf79600402c167cec39d3b63be86daa1f3756b52a82 | 52ed137b039cd9c46409bc22e89938af911c95b157feae2d040b51e6084369a7 |
| arborium-javascript | 47b63db8c449ac227061f2bb4ed62673d85c3f567be8f01483b34f266298d061 | 2e0110e07abef7c2548b26ec9d6969775617ca539a0dc8dbeeb14d6452c711d1 |
| arborium-json | dd1167c5abde6c8d953cc20d88713006c94a009926d39e2217c68cbffbdbf7f0 | 2e0110e07abef7c2548b26ec9d6969775617ca539a0dc8dbeeb14d6452c711d1 |
| arborium-kotlin | d2be9de4405957a7e6f9684d4774a8e743bb501ce5956d3e91da77e8e18a6fe6 | 948495f61768f7de26bcc61113d8cd95f50bbc15adb678c28c941c6c8fcd5903 |
| arborium-lua | d5ffae9459733aa8457f88e43672ccae485a77f09b15da83f8655d954c75365f | 9a32b02e4c917b1ce6b5e79d8ea81e25cefd7f27d89c7235f2afb262c06cf32e |
| arborium-markdown | f672968e3cddfa59a2b2b7777489078d9d20b220696891eeb35a9f20e51b9312 | 52ec8a1bf8256511e2a92613e0bb41ffefba3897f16fa958d7bb28c466dd4804 |
| arborium-php | 1772da0cd2f525332a958f5ebda27eda35be22dfa82b052774b6162b2c20b359 | 664764fabf40c0b318e06324a5a12379092a2df25b8cbf2415f71bc0bb352346 |
| arborium-python | 4da0da209e87d12f5922c5a7c005883d48d06baf0bf9b5d05bbe607b532e599f | d724405ce238a22c0d35769c5a36b386ad5958192efe8bbb304fb2896254575f |
| arborium-ruby | 9f791dc0377b8df659b98aed534dddfd4dcb591735cafd05b12e1e1ce3a859a9 | ee006f02a3d856df282e409be2a86e24a65bb573a98b9c28343771141351bb6b |
| arborium-bash | ab2a2c191c27ee915e5ae65b619f9f7c1345565d511ce7761d30b4b243b2e333 | 49bf33cf78ef5897e4e161ce1517df7de1ae5042a65b6bcfd44401e0fc606559 |
| arborium-sql | 67ca021dab5e61d6eb07bad9cf40f5f5e0fb4c6f89f32aa14a1db07431d657f9 | 3b3e4e4252d5d7d1c18e1257005f23242bf0580ad619204fd093c99b8c56748d |
| arborium-swift | db83273e075d168a853cb480805b0614803d43c20c7d5fc3f655dac836dd88e1 | 3533cec129bb4bba015c0d61d86dd7c3b7e82110e4d2ff7837a01eff5bad5ccc |
| arborium-toml | a9f6144d675fbfe2e510c5886955581b6838eb613a3d1af4250ddf10de5bfa5d | b727eee929bee836e8102cecf749a795b199f291b58c885bcd81cae727ac5a46 |
| arborium-typescript | 0024689641490077765b291be46197f48d23e75cbade86768b1abed64d406ed1 | 49bf33cf78ef5897e4e161ce1517df7de1ae5042a65b6bcfd44401e0fc606559 |
| arborium-tsx | d3533e40fe9f843290d4ff481c67ed1494595395fb60318dbc1da7e58bba42bb | 49bf33cf78ef5897e4e161ce1517df7de1ae5042a65b6bcfd44401e0fc606559 |
| arborium-xml | d01fb3f68edd5d901632cfbc9aa02ba537e16aafc3eda4603573acb2aeef7ae3 | 3297073719030b7261afd78b7267931ccc03a27f7a65ca6fc644316180f71bf6 |
| arborium-yaml | 528f9c28a2703384a66079b18f29e990f1ed749014a6556242e7c14ed5b29ea9 | 2d181298822a124f6d106118dcc69c8d4d26ac668559be29682c452e712b0947 |
| arborium-dockerfile | 54f567b2f106cea694126516b8ad20d44cddc40df8c16c77acf61ada303bfd9e | 806b8fcd19c1cdf23e178eb25942acc844a38669e36b2c896c7baf5f8bcecbb4 |
| arborium-make | 1bed9575f27d799d26b522d101f50abdb7f4f06d6a7065dbed41a6834b5dfe21 | 28eecdb75b6de2d9020603531461444bf54db827ee2fb345a8fcf4dbee7c2ad1 |
| arborium-ini | e7023ba09a01b681ebac2d78c5d1747d58b548ec3cb010e16f852fe8e62ed869 | c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4 |
| arborium-diff | 20f8e67cdf000ca1e099f71a28b32e17e9749fa27f2366c36eeb47c31ebee237 | bb98faf5a86e9458536a6c745e02b53775d01066f1e51dda4ae220a3c0b050f4 |
| arborium-batch | ff3235674b0e0e4aa992a1598874e33b6bc4e351931c15af37ff8347a79c1869 | 8f7339ea143af23c4075f6f000d5f8123011c7803b7ab120dd54ce2579b0a0bd |
| arborium-powershell | 33e087e691371a5b63c514c7a5c85c213932be5c52ada6107c5fb2314106be07 | 1fca8454c0d77a73f9922a52d39e1f7c339f5da5d03f2766eff66b859fa19477 |
| arborium-haskell | 67e7f4f0976568a87bedddeebd30724577674751fd275dab7c98c63616d9961c | 2e0110e07abef7c2548b26ec9d6969775617ca539a0dc8dbeeb14d6452c711d1 |
| arborium-scala | c02e14b416b76db7ede4935d5306e9ba5d42cd931396fb7ffc791de44ab46005 | 1f95ed26e1f4074074c9c7083e61c0a9e4c3b9f435745044995f3beb4ed28575 |
| arborium-jsdoc | 73945c45bdbbed8c137556d3a833999463e707078ca877597756dc394689e8b5 | ffaefecad68467134f8c701120a2410a2384ffff7a605b63638cf0b51e7d16cc |
| tree-sitter-rust | 439e577dbe07423ec2582ac62c7531120dbfccfa6e5f92406f93dd271a120e45 | 31d5b6f4243d5c7c6e1c4ebbbb9f6407bd1457a08bcc4f706521710341acba36 |
| kotlin-highlight-query | d2be9de4405957a7e6f9684d4774a8e743bb501ce5956d3e91da77e8e18a6fe6 | c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4 |
