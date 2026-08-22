# bgv-db benchmark

- backend: `memory`
- build: release build
- machine: `macos` / `aarch64`
- records per workload: 2000
- percentiles: nearest-rank over every retained sample, per phase

A baseline is comparable with another taken on the same machine and the same
build profile, and with nothing else. It is read by a person; it is not a gate.

## write

point writes of a small record, one statement each

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| write | 2000 | 101539 | 9.5 | 11.0 | 15.7 | 129.1 |

## read-by-id

point reads by record identity — the cheapest access path there is

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| write | 2000 | 115703 | 8.4 | 8.7 | 14.9 | 76.8 |
| read-by-id | 2000 | 313415 | 3.1 | 3.2 | 5.6 | 79.9 |

## filter

the same equality filter over a scan and over an index, so the two are one table

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| write | 2000 | 119918 | 8.1 | 8.5 | 11.2 | 93.8 |
| filter-scan | 100 | 1145 | 860.7 | 906.0 | 1024.5 | 1250.0 |
| filter-build-index | 1 | 481 | 2079.1 | 2079.1 | 2079.1 | 2079.1 |
| filter-index | 100 | 18365 | 53.5 | 55.8 | 61.7 | 86.3 |
| range-scan | 100 | 1172 | 847.9 | 891.4 | 930.7 | 1010.5 |
| range-build-index | 1 | 499 | 2002.7 | 2002.7 | 2002.7 | 2002.7 |
| range-index | 100 | 9517 | 104.2 | 108.2 | 115.8 | 154.8 |

## range

an index range at four widths, with what the process holds at each

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| range-write | 50000 | 96527 | 10.2 | 10.7 | 12.9 | 125.8 |
|   resident before | **46496 KiB** | | | | | |
| range-100 | 100 | 12076 | 81.2 | 84.4 | 124.5 | 127.7 |
|   served by | **index — 100 record(s) of 100 asked** | | | | | |
|   resident | **46560 KiB (+64 KiB since before the reads)** | | | | | |
| range-5000 | 100 | 276 | 3571.2 | 3854.9 | 4007.6 | 4242.4 |
|   served by | **index — 5000 record(s) of 5000 asked** | | | | | |
|   resident | **51360 KiB (+4864 KiB since before the reads)** | | | | | |
| range-25000 | 20 | 51 | 19420.1 | 20717.7 | 21926.1 | 21926.1 |
|   served by | **index — 25000 record(s) of 25000 asked** | | | | | |
|   resident | **77776 KiB (+31280 KiB since before the reads)** | | | | | |
| range-50000 | 10 | 23 | 43624.4 | 46319.8 | 47486.9 | 47486.9 |
|   served by | **index — 50000 record(s) of 50000 asked** | | | | | |
|   resident | **114224 KiB (+67728 KiB since before the reads)** | | | | | |

## search

a term search over a full-text index, against the scan of the same condition

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| search-write | 2000 | 101537 | 9.6 | 10.2 | 14.7 | 51.4 |
| search-scan | 100 | 626 | 1590.0 | 1619.8 | 1701.2 | 2178.5 |
| search-build-index | 1 | 133 | 7512.5 | 7512.5 | 7512.5 | 7512.5 |
| search-index | 100 | 129793 | 7.5 | 7.8 | 10.1 | 21.8 |
| search-rank | 100 | 77467 | 12.6 | 13.1 | 18.9 | 21.1 |

## capacity

sustained writes in escalating batches, with p99 and resident memory per batch

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| batch 1 (2500 records) | 2500 | 119898 | 8.2 | 8.5 | 10.5 | 39.2 |
|   resident | **104688 KiB after 2500 records** | | | | | |
| batch 2 (5000 records) | 2500 | 115951 | 8.3 | 8.8 | 14.9 | 109.5 |
|   resident | **104688 KiB after 5000 records** | | | | | |
| batch 3 (7500 records) | 2500 | 116616 | 8.4 | 8.8 | 11.3 | 46.1 |
|   resident | **104688 KiB after 7500 records** | | | | | |
| batch 4 (10000 records) | 2500 | 117668 | 8.4 | 8.8 | 10.8 | 27.0 |
|   resident | **104704 KiB after 10000 records** | | | | | |
| batch 5 (12500 records) | 2500 | 116638 | 8.3 | 8.8 | 11.2 | 24.5 |
|   resident | **104704 KiB after 12500 records** | | | | | |
| batch 6 (15000 records) | 2500 | 116167 | 8.4 | 8.9 | 11.1 | 32.0 |
|   resident | **104704 KiB after 15000 records** | | | | | |
| batch 7 (17500 records) | 2500 | 115645 | 8.4 | 9.0 | 11.3 | 23.8 |
|   resident | **104720 KiB after 17500 records** | | | | | |
| batch 8 (20000 records) | 2500 | 115610 | 8.4 | 9.0 | 11.9 | 47.2 |
|   resident | **104720 KiB after 20000 records** | | | | | |
| batch 9 (22500 records) | 2500 | 115428 | 8.4 | 9.0 | 12.1 | 45.3 |
|   resident | **104720 KiB after 22500 records** | | | | | |
| batch 10 (25000 records) | 2500 | 118032 | 8.4 | 8.7 | 10.2 | 22.1 |
|   resident | **104720 KiB after 25000 records** | | | | | |
| batch 11 (27500 records) | 2500 | 117139 | 8.4 | 8.8 | 10.2 | 19.5 |
|   resident | **105504 KiB after 27500 records** | | | | | |
| batch 12 (30000 records) | 2500 | 114220 | 8.5 | 9.0 | 13.7 | 46.0 |
|   resident | **106288 KiB after 30000 records** | | | | | |
| batch 13 (32500 records) | 2500 | 116171 | 8.5 | 8.8 | 10.2 | 57.5 |
|   resident | **107072 KiB after 32500 records** | | | | | |
| batch 14 (35000 records) | 2500 | 115808 | 8.5 | 8.8 | 10.6 | 28.8 |
|   resident | **107872 KiB after 35000 records** | | | | | |
| batch 15 (37500 records) | 2500 | 116449 | 8.5 | 8.8 | 10.1 | 20.5 |
|   resident | **108656 KiB after 37500 records** | | | | | |
| batch 16 (40000 records) | 2500 | 113904 | 8.6 | 9.0 | 13.5 | 41.2 |
|   resident | **109440 KiB after 40000 records** | | | | | |
| batch 17 (42500 records) | 2500 | 111660 | 8.8 | 9.2 | 12.1 | 49.9 |
|   resident | **110224 KiB after 42500 records** | | | | | |
| batch 18 (45000 records) | 2500 | 112720 | 8.8 | 9.0 | 10.5 | 34.7 |
|   resident | **111008 KiB after 45000 records** | | | | | |
| batch 19 (47500 records) | 2500 | 112720 | 8.8 | 9.0 | 10.4 | 27.3 |
|   resident | **111808 KiB after 47500 records** | | | | | |
| batch 20 (50000 records) | 2500 | 113065 | 8.8 | 9.0 | 10.3 | 25.4 |
|   resident | **112592 KiB after 50000 records** | | | | | |
| batch 21 (52500 records) | 2500 | 112638 | 8.8 | 9.0 | 11.5 | 44.2 |
|   resident | **113504 KiB after 52500 records** | | | | | |
| batch 22 (55000 records) | 2500 | 113035 | 8.7 | 9.1 | 12.0 | 63.7 |
|   resident | **114400 KiB after 55000 records** | | | | | |
| batch 23 (57500 records) | 2500 | 115040 | 8.5 | 8.9 | 11.0 | 28.5 |
|   resident | **115312 KiB after 57500 records** | | | | | |
| batch 24 (60000 records) | 2500 | 114553 | 8.5 | 8.9 | 11.9 | 76.5 |
|   resident | **116240 KiB after 60000 records** | | | | | |
| batch 25 (62500 records) | 2500 | 111103 | 8.6 | 9.4 | 14.6 | 173.5 |
|   resident | **117152 KiB after 62500 records** | | | | | |
| batch 26 (65000 records) | 2500 | 111925 | 8.7 | 9.5 | 12.1 | 39.8 |
|   resident | **118048 KiB after 65000 records** | | | | | |
| batch 27 (67500 records) | 2500 | 114528 | 8.5 | 8.8 | 12.0 | 66.6 |
|   resident | **118928 KiB after 67500 records** | | | | | |
| batch 28 (70000 records) | 2500 | 116230 | 8.5 | 8.8 | 10.4 | 26.1 |
|   resident | **119840 KiB after 70000 records** | | | | | |
| batch 29 (72500 records) | 2500 | 115511 | 8.5 | 9.0 | 10.2 | 49.0 |
|   resident | **120736 KiB after 72500 records** | | | | | |
| batch 30 (75000 records) | 2500 | 114832 | 8.5 | 9.0 | 10.4 | 47.1 |
|   resident | **121648 KiB after 75000 records** | | | | | |
| batch 31 (77500 records) | 2500 | 115670 | 8.5 | 8.9 | 10.3 | 26.5 |
|   resident | **122560 KiB after 77500 records** | | | | | |
| batch 32 (80000 records) | 2500 | 113309 | 8.6 | 9.1 | 13.2 | 31.1 |
|   resident | **123472 KiB after 80000 records** | | | | | |
| batch 33 (82500 records) | 2500 | 116039 | 8.5 | 8.9 | 10.0 | 25.8 |
|   resident | **124384 KiB after 82500 records** | | | | | |
| batch 34 (85000 records) | 2500 | 111874 | 8.6 | 9.1 | 15.2 | 100.0 |
|   resident | **125296 KiB after 85000 records** | | | | | |
| batch 35 (87500 records) | 2500 | 114568 | 8.6 | 9.0 | 10.6 | 34.0 |
|   resident | **126208 KiB after 87500 records** | | | | | |
| batch 36 (90000 records) | 2500 | 115029 | 8.5 | 8.9 | 10.4 | 56.8 |
|   resident | **127088 KiB after 90000 records** | | | | | |
| batch 37 (92500 records) | 2500 | 115743 | 8.5 | 8.9 | 10.5 | 26.0 |
|   resident | **128000 KiB after 92500 records** | | | | | |
| batch 38 (95000 records) | 2500 | 113735 | 8.6 | 9.1 | 10.5 | 35.4 |
|   resident | **128896 KiB after 95000 records** | | | | | |
| batch 39 (97500 records) | 2500 | 112906 | 8.6 | 9.1 | 12.8 | 35.2 |
|   resident | **130032 KiB after 97500 records** | | | | | |
| batch 40 (100000 records) | 2500 | 115899 | 8.5 | 8.9 | 10.5 | 28.2 |
|   resident | **131488 KiB after 100000 records** | | | | | |
| batch 41 (102500 records) | 2500 | 115376 | 8.5 | 8.8 | 11.6 | 25.1 |
|   resident | **132976 KiB after 102500 records** | | | | | |
| batch 42 (105000 records) | 2500 | 113778 | 8.5 | 9.1 | 13.2 | 79.3 |
|   resident | **134464 KiB after 105000 records** | | | | | |
| batch 43 (107500 records) | 2500 | 113450 | 8.5 | 9.0 | 13.4 | 65.1 |
|   resident | **136000 KiB after 107500 records** | | | | | |
| batch 44 (110000 records) | 2500 | 112006 | 8.6 | 9.3 | 12.8 | 97.0 |
|   resident | **137504 KiB after 110000 records** | | | | | |
| batch 45 (112500 records) | 2500 | 114432 | 8.6 | 9.0 | 11.3 | 29.3 |
|   resident | **139024 KiB after 112500 records** | | | | | |
| batch 46 (115000 records) | 2500 | 114582 | 8.6 | 8.9 | 10.5 | 41.7 |
|   resident | **140544 KiB after 115000 records** | | | | | |
| batch 47 (117500 records) | 2500 | 115341 | 8.5 | 8.9 | 10.3 | 46.7 |
|   resident | **142048 KiB after 117500 records** | | | | | |
| batch 48 (120000 records) | 2500 | 115499 | 8.5 | 8.8 | 10.4 | 36.3 |
|   resident | **143568 KiB after 120000 records** | | | | | |
| batch 49 (122500 records) | 2500 | 115735 | 8.5 | 8.8 | 10.0 | 21.1 |
|   resident | **145088 KiB after 122500 records** | | | | | |
| batch 50 (125000 records) | 2500 | 115218 | 8.5 | 8.9 | 10.5 | 23.6 |
|   resident | **146592 KiB after 125000 records** | | | | | |
| batch 51 (127500 records) | 2500 | 113720 | 8.6 | 8.9 | 12.6 | 107.5 |
|   resident | **148096 KiB after 127500 records** | | | | | |
| batch 52 (130000 records) | 2500 | 110925 | 8.6 | 10.0 | 13.5 | 100.6 |
|   resident | **149600 KiB after 130000 records** | | | | | |
| batch 53 (132500 records) | 2500 | 110683 | 8.6 | 10.6 | 12.6 | 75.3 |
|   resident | **151120 KiB after 132500 records** | | | | | |
| batch 54 (135000 records) | 2500 | 114303 | 8.5 | 9.0 | 12.2 | 54.5 |
|   resident | **152624 KiB after 135000 records** | | | | | |
| batch 55 (137500 records) | 2500 | 115494 | 8.5 | 8.8 | 10.5 | 28.5 |
|   resident | **154160 KiB after 137500 records** | | | | | |
| batch 56 (140000 records) | 2500 | 114846 | 8.5 | 9.0 | 10.3 | 50.3 |
|   resident | **155664 KiB after 140000 records** | | | | | |
| batch 57 (142500 records) | 2500 | 111152 | 8.8 | 9.1 | 11.6 | 50.9 |
|   resident | **157184 KiB after 142500 records** | | | | | |
| batch 58 (145000 records) | 2500 | 111669 | 8.8 | 9.1 | 12.7 | 29.4 |
|   resident | **158688 KiB after 145000 records** | | | | | |
| batch 59 (147500 records) | 2500 | 111002 | 8.9 | 9.2 | 11.0 | 77.1 |
|   resident | **160176 KiB after 147500 records** | | | | | |
| batch 60 (150000 records) | 2500 | 111634 | 8.8 | 9.1 | 10.5 | 31.7 |
|   resident | **161696 KiB after 150000 records** | | | | | |
| batch 61 (152500 records) | 2500 | 112376 | 8.8 | 9.1 | 10.9 | 31.9 |
|   resident | **163216 KiB after 152500 records** | | | | | |
| batch 62 (155000 records) | 2500 | 111158 | 8.8 | 9.2 | 13.0 | 42.3 |
|   resident | **164720 KiB after 155000 records** | | | | | |
| batch 63 (157500 records) | 2500 | 111948 | 8.7 | 9.2 | 13.3 | 79.1 |
|   resident | **166224 KiB after 157500 records** | | | | | |
| batch 64 (160000 records) | 2500 | 109660 | 8.7 | 9.3 | 17.2 | 116.5 |
|   resident | **167760 KiB after 160000 records** | | | | | |
| batch 65 (162500 records) | 2500 | 112461 | 8.6 | 9.5 | 12.1 | 38.7 |
|   resident | **169264 KiB after 162500 records** | | | | | |
| batch 66 (165000 records) | 2500 | 110224 | 8.8 | 9.4 | 11.2 | 96.8 |
|   resident | **170784 KiB after 165000 records** | | | | | |
| batch 67 (167500 records) | 2500 | 111257 | 8.6 | 9.2 | 14.0 | 86.7 |
|   resident | **172288 KiB after 167500 records** | | | | | |
| batch 68 (170000 records) | 2500 | 115015 | 8.5 | 8.9 | 10.5 | 31.4 |
|   resident | **173808 KiB after 170000 records** | | | | | |
| batch 69 (172500 records) | 2500 | 114381 | 8.6 | 8.9 | 10.8 | 44.7 |
|   resident | **175312 KiB after 172500 records** | | | | | |
| batch 70 (175000 records) | 2500 | 113662 | 8.6 | 9.1 | 11.2 | 33.1 |
|   resident | **176816 KiB after 175000 records** | | | | | |
| batch 71 (177500 records) | 2500 | 115064 | 8.5 | 8.9 | 10.3 | 30.1 |
|   resident | **178320 KiB after 177500 records** | | | | | |
| batch 72 (180000 records) | 2500 | 114193 | 8.6 | 8.9 | 10.8 | 71.6 |
|   resident | **179824 KiB after 180000 records** | | | | | |
| batch 73 (182500 records) | 2500 | 112057 | 8.6 | 10.6 | 12.0 | 26.8 |
|   resident | **181376 KiB after 182500 records** | | | | | |
| batch 74 (185000 records) | 2500 | 113063 | 8.6 | 9.1 | 13.2 | 46.1 |
|   resident | **182880 KiB after 185000 records** | | | | | |
| batch 75 (187500 records) | 2500 | 112908 | 8.5 | 9.0 | 14.0 | 47.8 |
|   resident | **184384 KiB after 187500 records** | | | | | |
| batch 76 (190000 records) | 2500 | 113645 | 8.6 | 9.1 | 10.4 | 57.1 |
|   resident | **185888 KiB after 190000 records** | | | | | |
| batch 77 (192500 records) | 2500 | 111680 | 8.7 | 9.2 | 12.3 | 38.0 |
|   resident | **187408 KiB after 192500 records** | | | | | |
| batch 78 (195000 records) | 2500 | 110981 | 8.8 | 9.3 | 12.6 | 83.4 |
|   resident | **188912 KiB after 195000 records** | | | | | |
| batch 79 (197500 records) | 2500 | 110763 | 8.7 | 9.2 | 13.8 | 111.8 |
|   resident | **190416 KiB after 197500 records** | | | | | |
| batch 80 (200000 records) | 2500 | 114552 | 8.6 | 9.0 | 10.9 | 24.7 |
|   resident | **191920 KiB after 200000 records** | | | | | |
| batch 81 (202500 records) | 2500 | 114622 | 8.6 | 8.9 | 10.7 | 26.8 |
|   resident | **193424 KiB after 202500 records** | | | | | |
| batch 82 (205000 records) | 2500 | 112513 | 8.6 | 9.0 | 13.7 | 57.5 |
|   resident | **194960 KiB after 205000 records** | | | | | |
| batch 83 (207500 records) | 2500 | 110265 | 8.8 | 9.2 | 16.4 | 56.8 |
|   resident | **196480 KiB after 207500 records** | | | | | |
| batch 84 (210000 records) | 2500 | 113365 | 8.6 | 8.9 | 13.0 | 82.7 |
|   resident | **197968 KiB after 210000 records** | | | | | |
| batch 85 (212500 records) | 2500 | 113630 | 8.6 | 9.0 | 11.3 | 65.0 |
|   resident | **199488 KiB after 212500 records** | | | | | |
| batch 86 (215000 records) | 2500 | 114281 | 8.6 | 8.9 | 10.7 | 25.5 |
|   resident | **200976 KiB after 215000 records** | | | | | |
| batch 87 (217500 records) | 2500 | 112667 | 8.8 | 9.2 | 10.5 | 37.6 |
|   resident | **202528 KiB after 217500 records** | | | | | |
| batch 88 (220000 records) | 2500 | 108376 | 8.8 | 9.2 | 15.5 | 127.4 |
|   resident | **204000 KiB after 220000 records** | | | | | |
| batch 89 (222500 records) | 2500 | 107889 | 8.7 | 9.0 | 13.2 | 265.5 |
|   resident | **205536 KiB after 222500 records** | | | | | |
| batch 90 (225000 records) | 2500 | 113898 | 8.6 | 9.0 | 11.4 | 31.3 |
|   resident | **207024 KiB after 225000 records** | | | | | |
| batch 91 (227500 records) | 2500 | 113947 | 8.6 | 9.0 | 10.6 | 21.5 |
|   resident | **208576 KiB after 227500 records** | | | | | |
| batch 92 (230000 records) | 2500 | 111925 | 8.7 | 9.2 | 13.5 | 38.9 |
|   resident | **210064 KiB after 230000 records** | | | | | |
| batch 93 (232500 records) | 2500 | 111035 | 8.7 | 9.2 | 16.2 | 71.1 |
|   resident | **211600 KiB after 232500 records** | | | | | |
| batch 94 (235000 records) | 2500 | 114050 | 8.6 | 9.0 | 10.9 | 41.4 |
|   resident | **213104 KiB after 235000 records** | | | | | |
| batch 95 (237500 records) | 2500 | 114296 | 8.6 | 9.0 | 10.6 | 43.3 |
|   resident | **214608 KiB after 237500 records** | | | | | |
| batch 96 (240000 records) | 2500 | 95277 | 9.0 | 9.6 | 54.6 | 276.8 |
|   resident | **216112 KiB after 240000 records** | | | | | |
| batch 97 (242500 records) | 2500 | 114876 | 8.6 | 8.9 | 10.1 | 19.5 |
|   resident | **217616 KiB after 242500 records** | | | | | |
| batch 98 (245000 records) | 2500 | 113945 | 8.6 | 8.9 | 10.4 | 42.4 |
|   resident | **219136 KiB after 245000 records** | | | | | |
| batch 99 (247500 records) | 2500 | 113670 | 8.6 | 8.9 | 10.2 | 47.2 |
|   resident | **220640 KiB after 247500 records** | | | | | |
| batch 100 (250000 records) | 2500 | 113593 | 8.7 | 9.0 | 10.2 | 40.0 |
|   resident | **222176 KiB after 250000 records** | | | | | |

## restore

a backup and the restore that replays it — the readiness row that has to be timed

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| backup | 1 | 1032 | 969.2 | 969.2 | 969.2 | 969.2 |
| backup size | **2004 record(s), 187406 bytes** | | | | | |
| restore | 1 | 99 | 10066.2 | 10066.2 | 10066.2 | 10066.2 |
| restored | **2004 record(s), truncated: false** | | | | | |

## vector-index

the same read served by a graph, with the recall it buys against the exact scan

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| vector-index-build | 1 | 6 | 157431.8 | 157431.8 | 157431.8 | 157431.8 |
| vector-exact-scan | 100 | 276 | 3574.5 | 3787.9 | 4062.9 | 4333.2 |
| vector-graph-walk | 100 | 1627 | 605.4 | 653.9 | 722.0 | 762.8 |
| recall | **100.0% of the exact ten, over 1000 asked** | | | | | |

## vector

a nearest-neighbour read over a scan — the number an HNSW index has to beat

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| vector-write | 2000 | 67812 | 14.6 | 15.0 | 17.8 | 97.8 |
| vector-nearest-scan | 100 | 266 | 3748.7 | 3985.5 | 4399.8 | 4434.8 |
