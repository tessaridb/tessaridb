# DEFINE SEARCH against the judgment set — 2026-10-01, macos-aarch64

Same corpus (fingerprint `8e7d29719dfe0da0`) and judgments as `2026-10-01-macos-aarch64-relevance.md`, release build,
memory store. Three runs of `cargo run -p tessari-bench --release --example relevance -- <docs content> benchmarks/judgments/docs.tsv`:
no flag (the field index over one combined `text` field), `--engine flat` (`DEFINE SEARCH` over title, heading, body,
weights 1) and `--engine weighted` (title 2, heading 3, body 1).

| Mode | all NDCG@10 | all MRR@10 | word NDCG@10 | prefix NDCG@10 | fuzzy NDCG@10 | cold p50 / p99 | warm p50 / p99 |
|---|---|---|---|---|---|---|---|
| field index | 0.6951 | 0.6936 | 0.8346 | 0.8018 | 0.1077 | 0.121 / 6.672 ms | 0.095 / 6.580 ms |
| `DEFINE SEARCH` flat | 0.7703 | 0.7608 | 0.8458 | 0.8090 | 0.4675 | 2.561 / 22.789 ms | 2.524 / 22.670 ms |
| `DEFINE SEARCH` weighted | 0.8187 | 0.8287 | 0.8946 | 0.8383 | 0.5300 | 2.547 / 22.703 ms | 2.533 / 23.291 ms |

The engine scores BM25F from each candidate's text, analysed once for the re-test and the score (ADR-0105 D2), so a
read pays the analyzer — the stemmer above all — on every candidate's three fields. That is the latency difference;
the ranking difference is the fields scored as one document (word, prefix), weights (weighted), and a fuzzy read that
is ranked rather than answered in store order (fuzzy).

## The weighted run, per query

```
build 0.20.0-beta release · macos-aarch64 · memory store · corpus 8e7d29719dfe0da0: 67 pages, 650 fragments · 81 queries · DEFINE SEARCH weighted
word   analyzer                     ndcg 0.854 rr 1.000  query-language/full-text-search reference/data-types overview/what-it-is overview/how-this-site-works query-language/geometry query-language/several-orders-at-once overview/engines query-language/definitions operations/backup-and-restore index
word   stemmer                      ndcg 1.000 rr 1.000  query-language/full-text-search overview/how-this-site-works
word   full text search             ndcg 0.893 rr 1.000  query-language/full-text-search reference/refusals query-language/conditions reference/functions reference/data-types overview/how-this-site-works index overview/engines overview/agent-memory query-language/several-orders-at-once
word   vector index                 ndcg 0.972 rr 1.000  query-language/vectors overview/what-it-is overview/engines reference/data-types query-language/definitions query-language/geometry reference/statements query-language/full-text-search index query-language/queues
word   approximate                  ndcg 1.000 rr 1.000  query-language/vectors overview/what-it-is overview/engines query-language/definitions query-language/several-orders-at-once query-language/what-a-read-reports query-language/records index
word   backup                       ndcg 0.605 rr 0.500  operations/the-console operations/backup-and-restore operations/serving reference/statements reference/command-line query-language/full-text-search security/users clients/http cluster/adding-a-node start/in-a-container
word   restore                      ndcg 0.956 rr 1.000  operations/backup-and-restore operations/the-console reference/command-line operations/serving security/users start/in-a-container start/install cluster/two-writers-on-one-range cluster/adding-a-node operations/bounding-the-log
word   snapshot                     ndcg 0.580 rr 0.500  overview/engines operations/backup-and-restore operations/the-console operations/bounding-the-log index reference/refusals key-value/expiry-and-counters clients/http query-language/paging-and-history reference/statements
word   transaction                  ndcg 0.618 rr 0.500  reference/refusals query-language/transactions reference/statements query-language/paging-and-history query-language/writing-safely overview/engines reference/command-line cluster/what-a-cluster-is query-language/queues index
word   failover                     ndcg 0.983 rr 1.000  cluster/leadership-and-failover cluster/reads-routing-and-staleness cluster/what-a-cluster-is reference/statements cluster/two-writers-on-one-range cluster/splitting-a-table reference/refusals index operations/the-console
word   leader lease                 ndcg 0.917 rr 1.000  cluster/leadership-and-failover cluster/splitting-a-table reference/refusals cluster/two-writers-on-one-range reference/statements
word   staleness                    ndcg 0.983 rr 1.000  cluster/reads-routing-and-staleness clients/python clients/rust cluster/what-a-cluster-is cluster/adding-a-node cluster/what-replicates-where reference/statements cluster/leadership-and-failover clients/kotlin clients/javascript
word   shard                        ndcg 0.917 rr 1.000  cluster/splitting-a-table operations/backup-and-restore reference/refusals cluster/what-a-cluster-is query-language/topics reference/statements
word   replication                  ndcg 0.946 rr 1.000  cluster/what-replicates-where cluster/two-writers-on-one-range reference/statements query-language/definitions cluster/splitting-a-table cluster/adding-a-node cluster/what-a-cluster-is security/authority operations/bounding-the-log key-value/expiry-and-counters
word   password                     ndcg 0.691 rr 0.500  reference/command-line security/users start/connecting clients/http clients/javascript start/in-a-container security/authority operations/the-console clients/kotlin clients/go
word   grant                        ndcg 0.936 rr 1.000  security/grants security/users query-language/views cluster/adding-a-node security/authority reference/statements reference/refusals start/connecting cluster/leadership-and-failover query-language/stream-ingestion
word   token                        ndcg 0.947 rr 1.000  clients/http security/users query-language/vaults key-value/spaces query-language/full-text-search query-language/parameters clients/javascript query-language/transactions security/authority reference/refusals
word   queue claim                  ndcg 1.000 rr 1.000  query-language/queues reference/statements reference/refusals overview/engines index
word   dead letter                  ndcg 1.000 rr 1.000  query-language/queues query-language/stream-ingestion operations/the-console reference/refusals query-language/topics reference/statements operations/serving
word   topic consumer               ndcg 0.586 rr 1.000  query-language/stream-ingestion clients/javascript clients/kotlin clients/python clients/rust clients/go reference/statements query-language/topics reference/refusals operations/serving
word   kafka                        ndcg 1.000 rr 1.000  query-language/stream-ingestion reference/statements query-language/definitions reference/refusals
word   expiry                       ndcg 0.988 rr 1.000  key-value/expiry-and-counters key-value/a-cache-over-http-and-in-the-clients reference/statements clients/go key-value/spaces reference/refusals query-language/queues clients/kotlin clients/javascript clients/rust
word   counter                      ndcg 0.972 rr 1.000  key-value/expiry-and-counters query-language/time-series key-value/a-cache-over-http-and-in-the-clients key-value/spaces reference/statements operations/the-console cluster/two-writers-on-one-range query-language/records overview/what-it-is query-language/documents
word   lock                         ndcg 0.834 rr 1.000  key-value/a-cache-over-http-and-in-the-clients key-value/expiry-and-counters query-language/several-orders-at-once clients/go clients/kotlin clients/javascript clients/rust clients/python query-language/full-text-search reference/statements
word   bucket file                  ndcg 1.000 rr 1.000  key-value/files clients/http reference/refusals clients/javascript index query-language/definitions overview/engines overview/what-it-is clients/kotlin reference/statements
word   polygon                      ndcg 0.917 rr 1.000  query-language/geometry reference/refusals reference/functions
word   geometry distance            ndcg 0.975 rr 1.000  query-language/geometry reference/data-types query-language/several-orders-at-once reference/functions query-language/definitions overview/engines
word   graph edge                   ndcg 1.000 rr 1.000  query-language/graphs reference/refusals overview/how-this-site-works query-language/definitions key-value/files query-language/joins query-language/vectors overview/engines index query-language/queues
word   relate                       ndcg 0.917 rr 1.000  query-language/graphs key-value/files query-language/geometry query-language/joins cluster/splitting-a-table query-language/definitions cluster/reads-routing-and-staleness reference/refusals operations/backup-and-restore overview/engines
word   join                         ndcg 0.665 rr 0.500  reference/refusals query-language/joins query-language/subqueries query-language/time-series cluster/adding-a-node query-language/expressions query-language/graphs query-language/stream-ingestion index query-language/vectors
word   subquery                     ndcg 0.000 rr 0.000  query-language/bindings query-language/conditions query-language/what-a-read-reports query-language/topics cluster/splitting-a-table
word   view                         ndcg 1.000 rr 1.000  query-language/views reference/refusals query-language/projections query-language/geometry operations/the-console cluster/reads-routing-and-staleness reference/statements
word   materialized view            ndcg 0.917 rr 1.000  query-language/views
word   vault secret                 ndcg 0.917 rr 1.000  query-language/vaults reference/refusals clients/kotlin clients/python clients/javascript clients/go clients/rust index overview/engines reference/statements
word   unseal                       ndcg 0.917 rr 1.000  query-language/vaults operations/serving clients/javascript clients/kotlin clients/python clients/go clients/rust reference/statements start/in-a-container clients/http
word   time series window           ndcg 1.000 rr 1.000  query-language/time-series reference/refusals index query-language/views overview/engines reference/statements
word   parameter                    ndcg 0.940 rr 1.000  query-language/parameters reference/refusals query-language/bindings reference/command-line clients/rust clients/kotlin clients/python clients/protocol clients/http clients/javascript
word   explain                      ndcg 0.834 rr 1.000  query-language/conditions query-language/what-a-read-reports query-language/vaults query-language/full-text-search overview/how-this-site-works cluster/what-a-cluster-is query-language/geometry cluster/splitting-a-table reference/statements start/in-a-container
word   paging                       ndcg 1.000 rr 1.000  query-language/paging-and-history overview/how-this-site-works reference/refusals query-language/vaults clients/kotlin clients/rust clients/python query-language/conditions index reference/functions
word   version history              ndcg 1.000 rr 1.000  query-language/paging-and-history reference/refusals cluster/reads-routing-and-staleness query-language/conditions operations/backup-and-restore overview/agent-memory cluster/what-a-cluster-is reference/statements index
word   container                    ndcg 1.000 rr 1.000  start/in-a-container operations/serving start/install query-language/expressions reference/data-types security/authority query-language/conditions query-language/bindings operations/backup-and-restore query-language/stream-ingestion
word   install                      ndcg 1.000 rr 1.000  start/install clients/choosing clients/python clients/javascript start/in-a-container index start/first-store operations/the-console cluster/leadership-and-failover reference/command-line
word   console                      ndcg 1.000 rr 1.000  operations/the-console start/in-a-container start/the-prompt query-language/vaults clients/http operations/backup-and-restore query-language/stream-ingestion operations/bounding-the-log clients/javascript
word   health probe                 ndcg 0.972 rr 1.000  operations/serving start/in-a-container clients/http clients/choosing
word   refusal                      ndcg 1.000 rr 1.000  reference/refusals query-language/full-text-search cluster/two-writers-on-one-range query-language/vaults key-value/files query-language/several-orders-at-once query-language/documents query-language/writing-safely cluster/reads-routing-and-staleness clients/choosing
word   null none                    ndcg 0.631 rr 1.000  query-language/expressions clients/python query-language/conditions key-value/spaces query-language/stream-ingestion reference/data-types operations/bounding-the-log key-value/expiry-and-counters reference/statements query-language/definitions
word   fuse                         ndcg 1.000 rr 1.000  query-language/several-orders-at-once reference/refusals query-language/geometry reference/functions overview/engines index overview/agent-memory
word   highlight                    ndcg 0.834 rr 1.000  reference/functions query-language/full-text-search
word   prune log                    ndcg 0.710 rr 0.500  operations/backup-and-restore operations/bounding-the-log index reference/refusals start/in-a-container
word   websocket                    ndcg 1.000 rr 1.000  clients/protocol clients/javascript clients/http operations/the-console clients/choosing cluster/splitting-a-table
word   python                       ndcg 0.983 rr 1.000  clients/python key-value/a-cache-over-http-and-in-the-clients clients/choosing query-language/time-series query-language/topics cluster/reads-routing-and-staleness
word   drain                        ndcg 1.000 rr 1.000  operations/serving cluster/leadership-and-failover start/in-a-container cluster/adding-a-node index operations/the-console cluster/splitting-a-table reference/refusals
word   multi master conflict        ndcg 1.000 rr 1.000  cluster/two-writers-on-one-range
word   agent memory                 ndcg 1.000 rr 1.000  overview/agent-memory index
prefix vect                         ndcg 0.964 rr 1.000  query-language/vectors reference/refusals reference/functions reference/data-types overview/what-it-is overview/engines query-language/full-text-search query-language/definitions clients/protocol query-language/several-orders-at-once
prefix analy                        ndcg 1.000 rr 1.000  query-language/full-text-search reference/data-types overview/what-it-is overview/how-this-site-works query-language/geometry query-language/several-orders-at-once index overview/engines query-language/definitions operations/backup-and-restore
prefix replica                      ndcg 0.586 rr 1.000  cluster/adding-a-node query-language/definitions operations/the-console operations/bounding-the-log cluster/splitting-a-table query-language/records reference/statements cluster/what-replicates-where operations/backup-and-restore cluster/reads-routing-and-staleness
prefix transac                      ndcg 0.622 rr 0.500  reference/refusals query-language/transactions reference/statements query-language/paging-and-history query-language/writing-safely overview/engines reference/command-line cluster/what-a-cluster-is query-language/queues index
prefix backu                        ndcg 0.629 rr 0.500  operations/the-console operations/backup-and-restore operations/serving reference/statements reference/command-line query-language/full-text-search security/users clients/http cluster/adding-a-node start/in-a-container
prefix geom                         ndcg 1.000 rr 1.000  query-language/geometry reference/data-types reference/refusals query-language/several-orders-at-once reference/functions index query-language/definitions clients/choosing clients/protocol overview/engines
prefix queu                         ndcg 1.000 rr 1.000  query-language/queues reference/statements reference/refusals query-language/topics index operations/serving overview/engines clients/rust overview/what-it-is operations/backup-and-restore
prefix stale                        ndcg 1.000 rr 1.000  cluster/reads-routing-and-staleness clients/python clients/rust cluster/what-a-cluster-is cluster/adding-a-node cluster/what-replicates-where reference/statements cluster/leadership-and-failover clients/kotlin clients/javascript
prefix subscr                       ndcg 0.627 rr 0.500  clients/python cluster/what-replicates-where cluster/splitting-a-table clients/go cluster/adding-a-node clients/kotlin clients/protocol query-language/topics operations/serving security/authority
prefix hist                         ndcg 1.000 rr 1.000  query-language/paging-and-history reference/refusals cluster/leadership-and-failover operations/backup-and-restore query-language/conditions start/the-prompt operations/the-console cluster/reads-routing-and-staleness overview/agent-memory operations/bounding-the-log
prefix seri                         ndcg 0.631 rr 0.500  reference/refusals query-language/time-series clients/http clients/rust clients/python clients/javascript clients/go clients/kotlin index query-language/views
prefix unsea                        ndcg 1.000 rr 1.000  query-language/vaults operations/serving clients/javascript clients/kotlin clients/python clients/go clients/rust reference/statements start/in-a-container clients/http
fuzzy  transcation                  ndcg 0.618 rr 0.500  reference/refusals query-language/transactions reference/statements query-language/paging-and-history query-language/writing-safely overview/engines reference/command-line cluster/what-a-cluster-is query-language/queues index
fuzzy  replciation                  ndcg 0.420 rr 0.125  cluster/adding-a-node query-language/definitions operations/the-console operations/bounding-the-log cluster/splitting-a-table query-language/records reference/statements cluster/what-replicates-where operations/backup-and-restore cluster/reads-routing-and-staleness
fuzzy  geomtery                     ndcg 1.000 rr 1.000  query-language/geometry reference/data-types reference/refusals query-language/several-orders-at-once reference/functions index query-language/definitions clients/choosing clients/protocol overview/engines
fuzzy  snapshto                     ndcg 0.631 rr 0.500  overview/engines operations/backup-and-restore operations/the-console operations/bounding-the-log index reference/refusals key-value/expiry-and-counters clients/http query-language/paging-and-history reference/statements
fuzzy  vecotrs                      ndcg 0.964 rr 1.000  query-language/vectors reference/functions reference/refusals reference/data-types overview/what-it-is overview/engines query-language/full-text-search query-language/definitions clients/protocol query-language/several-orders-at-once
fuzzy  failvoer                     ndcg 0.000 rr 0.000  
fuzzy  pasword                      ndcg 0.691 rr 0.500  reference/command-line security/users start/connecting clients/http clients/javascript start/in-a-container security/authority operations/the-console clients/kotlin clients/go
fuzzy  vaulst                       ndcg 1.000 rr 1.000  query-language/vaults clients/python clients/javascript clients/kotlin clients/rust clients/go operations/the-console reference/refusals clients/http reference/statements
fuzzy  unsael                       ndcg 1.000 rr 1.000  query-language/vaults operations/serving clients/javascript clients/kotlin clients/python clients/go start/in-a-container clients/rust reference/statements cluster/reads-routing-and-staleness
fuzzy  consoel                      ndcg 1.000 rr 1.000  operations/the-console start/in-a-container start/the-prompt query-language/vaults clients/http operations/backup-and-restore query-language/stream-ingestion operations/bounding-the-log clients/javascript
fuzzy  backpu                       ndcg 0.625 rr 0.500  operations/the-console operations/backup-and-restore operations/serving security/users reference/statements reference/command-line cluster/adding-a-node query-language/full-text-search clients/python start/the-prompt
fuzzy  trasnactoin                  ndcg 0.000 rr 0.000  
fuzzy  subscirptoin                 ndcg 0.000 rr 0.000  
fuzzy  replciatoin                  ndcg 0.000 rr 0.000  
fuzzy  anlayzer                     ndcg 0.000 rr 0.000  
all    NDCG@10 0.8187  MRR@10 0.8287  (81 queries)
fuzzy  NDCG@10 0.5300  MRR@10 0.4750  (15 queries)
prefix NDCG@10 0.8383  MRR@10 0.8333  (12 queries)
word   NDCG@10 0.8946  MRR@10 0.9259  (54 queries)
cold  p50 2.547 ms  p99 22.703 ms  (81 runs)
warm  p50 2.533 ms  p99 23.291 ms  (405 runs)
```
