# KW-28 word source freeze

2026-10-25. Work location `E:/Development/kwil`, main base `3092b71` plus
pre-existing shared dirt. Source candidate only; **full v1 is incomplete**.
ROOT accepted delivery with the explicit Similar32 audit gate on 2026-10-25.

| Owned file relative to crates/core-diff/src | SHA-256 |
| --- | --- |
| words.rs | D0A18862535ADBFF4CFB51B79713A14836CA8173CB10B24A283B1B730DAE86FF |
| words/tokens.rs | 69B119C872A98EA254B8500DFE31FDB2DB18AAD4FC846201332751DFCF913354 |
| words/worker.rs | 618B38B2FEEADFBD32E1C468B1FBA572CB8EC6B8C3C837624233660261DF9DFF |
| words/tests.rs | E742F8B186FA30129F31528BA97EAA8B112DFAB529A0548B17DB8EC0CE715859 |
| words/similar_audit.md | 392DD5AD18A13AE0E74B96255CB57351ED4DFC2F63F87AA2059715F9AE896E28 |
| words/README.md | 3D9CAD7544159D7B1F4F490FBA271766EAD1757BB290A10701718B208AB29D2E |

The freeze document's own hash is reported outside this document to avoid a
self-reference. No source file is edited after these hashes without a new
explicitly identified freeze.

Shared kernel dependency from its separate owner, inspected unchanged:

- `sequence.rs`: A24F15B8D869A13F899C9B3D1855F06BE27FE691C533A0505434A2A3B4F8BAF2
- `stream.rs`: D74C0E07DC41C49F0C8C368C18B041BBCE3A17B0EF8BD75DE56B33CA379EEC14
- Existing `stream_tests.rs`: A72EC26444AB48C4BD6F017C1866E23C469A464E9D9E827F4E88EE667BFFB6B8

The word package did not edit stream/store/lib/core-text/manifests or any
existing test. It consumes the shared associated-Equality SequencePair API,
Layout, held Event/acknowledge and transient_charge, plus the two immutable
comparison query/nonce seams approved by ROOT.

## Evidence and remaining gate

- Authored 15 focused source fixtures; actual Similar oracle branch is held
  behind the typed audit gate. No authored test or mutation was executed.
- Pinned standalone rustfmt 1.99.1 formatting: own PIDs 32684 and 57240,
  exited 0 within individual 10000ms bounds. Final standalone rustfmt --check:
  own PID 47960, exited 0 within 10000ms. All three owned processes exited.
- Read-only `git diff --check` for the tracked words.rs change: exit 0.
- **NOT RUN:** Cargo, rustc, tests, fail-first/mutation checks, strict lint,
  performance/memory/stack checks, native UI/player, network, Git mutation,
  delegation or integration. No persistent environment change.
- **Similar32 is not ready:** source cardinality/work/recursion audit is in
  similar_audit.md, but pinned allocator/BTree/Vec layout and compiled physical
  worker-stack/frame evidence are missing. `SIMILAR32_AUDIT_ACCEPTED=false`
  produces Pending/Similar32AuditRequired before the actual legacy call.
  Neither the candidate 16MiB heap envelope nor work credits bypass that gate.
- ROOT next: integrate its private sequence module, compile and run focused
  external fixtures independently, review actual artifact and source fences,
  then obtain and qualify the missing Similar32 evidence before enabling it.
  Native/hosted/product acceptance and full v1 closure remain ROOT-owned.
