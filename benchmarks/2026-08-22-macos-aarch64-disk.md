# TessariDB benchmark

- backend: `disk`
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
| write | 2000 | 17345 | 56.1 | 67.6 | 86.7 | 154.9 |

## read-by-id

point reads by record identity — the cheapest access path there is

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| write | 2000 | 19079 | 50.9 | 60.0 | 76.6 | 101.1 |
| read-by-id | 2000 | 117949 | 8.4 | 8.8 | 9.2 | 71.3 |

## filter

the same equality filter over a scan and over an index, so the two are one table

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| write | 2000 | 18993 | 51.5 | 59.6 | 71.5 | 112.5 |
| filter-scan | 100 | 945 | 1052.5 | 1088.4 | 1242.1 | 1411.7 |
| filter-build-index | 1 | 316 | 3162.3 | 3162.3 | 3162.3 | 3162.3 |
| filter-index | 100 | 9430 | 103.9 | 109.2 | 141.2 | 161.0 |
| range-scan | 100 | 968 | 1021.5 | 1102.1 | 1196.4 | 1214.8 |
| range-build-index | 1 | 306 | 3272.5 | 3272.5 | 3272.5 | 3272.5 |
| range-index | 100 | 3868 | 257.4 | 266.6 | 282.5 | 331.0 |

## range

an index range at four widths, with what the process holds at each

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| range-write | 50000 | 16103 | 59.5 | 68.3 | 100.5 | 1986.3 |
|   resident before | **37296 KiB** | | | | | |
| range-100 | 100 | 4798 | 206.0 | 221.4 | 253.8 | 281.0 |
|   served by | **index — 100 record(s) of 100 asked** | | | | | |
|   resident | **37328 KiB (+32 KiB since before the reads)** | | | | | |
| range-5000 | 100 | 105 | 9540.8 | 9904.8 | 10293.6 | 10480.4 |
|   served by | **index — 5000 record(s) of 5000 asked** | | | | | |
|   resident | **41936 KiB (+4640 KiB since before the reads)** | | | | | |
| range-25000 | 20 | 20 | 49126.7 | 50573.3 | 51050.2 | 51050.2 |
|   served by | **index — 25000 record(s) of 25000 asked** | | | | | |
|   resident | **71696 KiB (+34400 KiB since before the reads)** | | | | | |
| range-50000 | 10 | 10 | 101889.8 | 103969.8 | 105482.4 | 105482.4 |
|   served by | **index — 50000 record(s) of 50000 asked** | | | | | |
|   resident | **107504 KiB (+70208 KiB since before the reads)** | | | | | |

## search

a term search over a full-text index, against the scan of the same condition

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| search-write | 2000 | 18386 | 52.8 | 60.9 | 75.2 | 101.7 |
| search-scan | 100 | 570 | 1736.6 | 1792.7 | 1983.3 | 2003.6 |
| search-build-index | 1 | 85 | 11766.3 | 11766.3 | 11766.3 | 11766.3 |
| search-index | 100 | 56541 | 16.8 | 17.8 | 34.8 | 52.8 |
| search-rank | 100 | 40081 | 24.7 | 25.5 | 26.7 | 35.3 |

## capacity

sustained writes in escalating batches, with p99 and resident memory per batch

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| batch 1 (2500 records) | 2500 | 14945 | 56.3 | 63.7 | 87.1 | 16826.2 |
|   resident | **109472 KiB after 2500 records** | | | | | |
| batch 2 (5000 records) | 2500 | 17397 | 54.9 | 64.4 | 83.7 | 1975.8 |
|   resident | **109472 KiB after 5000 records** | | | | | |
| batch 3 (7500 records) | 2500 | 18289 | 52.7 | 62.9 | 82.0 | 178.2 |
|   resident | **109472 KiB after 7500 records** | | | | | |
| batch 4 (10000 records) | 2500 | 18581 | 51.9 | 62.6 | 75.6 | 134.5 |
|   resident | **109472 KiB after 10000 records** | | | | | |
| batch 5 (12500 records) | 2500 | 18685 | 51.8 | 62.2 | 77.3 | 106.2 |
|   resident | **109472 KiB after 12500 records** | | | | | |
| batch 6 (15000 records) | 2500 | 15867 | 53.0 | 62.8 | 77.8 | 14178.1 |
|   resident | **109472 KiB after 15000 records** | | | | | |
| batch 7 (17500 records) | 2500 | 18700 | 52.1 | 60.8 | 72.1 | 170.0 |
|   resident | **109472 KiB after 17500 records** | | | | | |
| batch 8 (20000 records) | 2500 | 18719 | 52.8 | 58.4 | 64.7 | 138.3 |
|   resident | **109488 KiB after 20000 records** | | | | | |
| batch 9 (22500 records) | 2500 | 18462 | 53.0 | 59.5 | 68.7 | 173.1 |
|   resident | **109568 KiB after 22500 records** | | | | | |
| batch 10 (25000 records) | 2500 | 18063 | 53.5 | 62.5 | 82.9 | 183.2 |
|   resident | **109712 KiB after 25000 records** | | | | | |
| batch 11 (27500 records) | 2500 | 18708 | 52.7 | 59.4 | 67.7 | 169.5 |
|   resident | **110272 KiB after 27500 records** | | | | | |
| batch 12 (30000 records) | 2500 | 18724 | 52.6 | 59.5 | 68.2 | 178.5 |
|   resident | **110448 KiB after 30000 records** | | | | | |
| batch 13 (32500 records) | 2500 | 18703 | 52.4 | 59.9 | 68.2 | 155.5 |
|   resident | **111008 KiB after 32500 records** | | | | | |
| batch 14 (35000 records) | 2500 | 18629 | 52.6 | 59.8 | 67.4 | 157.0 |
|   resident | **111328 KiB after 35000 records** | | | | | |
| batch 15 (37500 records) | 2500 | 18281 | 53.2 | 60.0 | 81.1 | 179.8 |
|   resident | **111424 KiB after 37500 records** | | | | | |
| batch 16 (40000 records) | 2500 | 18107 | 53.9 | 60.6 | 69.6 | 160.7 |
|   resident | **111504 KiB after 40000 records** | | | | | |
| batch 17 (42500 records) | 2500 | 18152 | 53.8 | 60.7 | 74.0 | 184.0 |
|   resident | **112128 KiB after 42500 records** | | | | | |
| batch 18 (45000 records) | 2500 | 18378 | 53.1 | 60.8 | 73.6 | 176.3 |
|   resident | **112768 KiB after 45000 records** | | | | | |
| batch 19 (47500 records) | 2500 | 18672 | 52.6 | 59.6 | 66.4 | 162.6 |
|   resident | **112848 KiB after 47500 records** | | | | | |
| batch 20 (50000 records) | 2500 | 18487 | 53.0 | 60.2 | 67.5 | 128.8 |
|   resident | **112928 KiB after 50000 records** | | | | | |
| batch 21 (52500 records) | 2500 | 18755 | 52.5 | 58.5 | 65.3 | 210.5 |
|   resident | **113008 KiB after 52500 records** | | | | | |
| batch 22 (55000 records) | 2500 | 18803 | 52.3 | 58.3 | 65.5 | 109.5 |
|   resident | **113104 KiB after 55000 records** | | | | | |
| batch 23 (57500 records) | 2500 | 18186 | 53.7 | 60.6 | 74.6 | 193.5 |
|   resident | **113184 KiB after 57500 records** | | | | | |
| batch 24 (60000 records) | 2500 | 17807 | 54.5 | 62.8 | 79.3 | 244.2 |
|   resident | **113264 KiB after 60000 records** | | | | | |
| batch 25 (62500 records) | 2500 | 18890 | 52.3 | 59.0 | 66.8 | 118.4 |
|   resident | **113280 KiB after 62500 records** | | | | | |
| batch 26 (65000 records) | 2500 | 18101 | 53.5 | 61.3 | 74.0 | 1060.4 |
|   resident | **113280 KiB after 65000 records** | | | | | |
| batch 27 (67500 records) | 2500 | 18778 | 52.4 | 59.6 | 67.1 | 163.1 |
|   resident | **113280 KiB after 67500 records** | | | | | |
| batch 28 (70000 records) | 2500 | 18483 | 53.0 | 60.0 | 66.6 | 116.4 |
|   resident | **113280 KiB after 70000 records** | | | | | |
| batch 29 (72500 records) | 2500 | 18613 | 52.9 | 59.1 | 65.8 | 151.8 |
|   resident | **113824 KiB after 72500 records** | | | | | |
| batch 30 (75000 records) | 2500 | 18440 | 53.3 | 59.8 | 69.7 | 145.5 |
|   resident | **114784 KiB after 75000 records** | | | | | |
| batch 31 (77500 records) | 2500 | 17916 | 54.3 | 61.8 | 77.7 | 145.3 |
|   resident | **115696 KiB after 77500 records** | | | | | |
| batch 32 (80000 records) | 2500 | 18637 | 52.6 | 59.8 | 68.6 | 160.2 |
|   resident | **116640 KiB after 80000 records** | | | | | |
| batch 33 (82500 records) | 2500 | 18720 | 52.7 | 58.3 | 64.5 | 93.7 |
|   resident | **117584 KiB after 82500 records** | | | | | |
| batch 34 (85000 records) | 2500 | 18899 | 52.3 | 57.8 | 64.7 | 121.5 |
|   resident | **118544 KiB after 85000 records** | | | | | |
| batch 35 (87500 records) | 2500 | 18478 | 53.2 | 59.2 | 67.9 | 165.4 |
|   resident | **119456 KiB after 87500 records** | | | | | |
| batch 36 (90000 records) | 2500 | 18824 | 52.6 | 58.1 | 63.3 | 110.0 |
|   resident | **120400 KiB after 90000 records** | | | | | |
| batch 37 (92500 records) | 2500 | 18004 | 55.0 | 60.2 | 67.8 | 140.8 |
|   resident | **121408 KiB after 92500 records** | | | | | |
| batch 38 (95000 records) | 2500 | 16948 | 58.5 | 63.9 | 76.2 | 353.2 |
|   resident | **122416 KiB after 95000 records** | | | | | |
| batch 39 (97500 records) | 2500 | 18415 | 53.0 | 59.8 | 73.1 | 244.7 |
|   resident | **123424 KiB after 97500 records** | | | | | |
| batch 40 (100000 records) | 2500 | 18783 | 52.5 | 58.7 | 64.8 | 106.4 |
|   resident | **124464 KiB after 100000 records** | | | | | |
| batch 41 (102500 records) | 2500 | 18830 | 52.5 | 57.8 | 65.7 | 170.5 |
|   resident | **125488 KiB after 102500 records** | | | | | |
| batch 42 (105000 records) | 2500 | 18262 | 53.7 | 60.6 | 70.3 | 160.4 |
|   resident | **126496 KiB after 105000 records** | | | | | |
| batch 43 (107500 records) | 2500 | 18752 | 52.8 | 58.2 | 64.0 | 116.5 |
|   resident | **127520 KiB after 107500 records** | | | | | |
| batch 44 (110000 records) | 2500 | 18418 | 52.9 | 59.7 | 72.9 | 187.5 |
|   resident | **128544 KiB after 110000 records** | | | | | |
| batch 45 (112500 records) | 2500 | 18369 | 53.5 | 59.9 | 68.2 | 196.1 |
|   resident | **129536 KiB after 112500 records** | | | | | |
| batch 46 (115000 records) | 2500 | 17172 | 54.5 | 64.7 | 116.0 | 723.1 |
|   resident | **130208 KiB after 115000 records** | | | | | |
| batch 47 (117500 records) | 2500 | 18793 | 52.5 | 59.2 | 66.1 | 147.4 |
|   resident | **130736 KiB after 117500 records** | | | | | |
| batch 48 (120000 records) | 2500 | 18169 | 53.1 | 60.8 | 73.1 | 872.8 |
|   resident | **131600 KiB after 120000 records** | | | | | |
| batch 49 (122500 records) | 2500 | 18487 | 53.0 | 60.5 | 67.6 | 216.2 |
|   resident | **132592 KiB after 122500 records** | | | | | |
| batch 50 (125000 records) | 2500 | 18375 | 53.2 | 60.0 | 72.8 | 174.6 |
|   resident | **133648 KiB after 125000 records** | | | | | |
| batch 51 (127500 records) | 2500 | 18164 | 53.5 | 61.2 | 73.5 | 170.5 |
|   resident | **134656 KiB after 127500 records** | | | | | |
| batch 52 (130000 records) | 2500 | 18162 | 54.1 | 60.5 | 69.5 | 168.6 |
|   resident | **135648 KiB after 130000 records** | | | | | |
| batch 53 (132500 records) | 2500 | 17875 | 54.0 | 62.7 | 91.0 | 173.0 |
|   resident | **136688 KiB after 132500 records** | | | | | |
| batch 54 (135000 records) | 2500 | 18424 | 53.1 | 60.5 | 68.3 | 471.2 |
|   resident | **140000 KiB after 135000 records** | | | | | |
| batch 55 (137500 records) | 2500 | 15307 | 62.5 | 73.2 | 112.5 | 966.8 |
|   resident | **143680 KiB after 137500 records** | | | | | |
| batch 56 (140000 records) | 2500 | 15261 | 63.3 | 75.1 | 94.0 | 222.9 |
|   resident | **144608 KiB after 140000 records** | | | | | |
| batch 57 (142500 records) | 2500 | 15516 | 63.2 | 72.6 | 86.2 | 146.5 |
|   resident | **145168 KiB after 142500 records** | | | | | |
| batch 58 (145000 records) | 2500 | 14487 | 65.7 | 75.3 | 92.4 | 1236.5 |
|   resident | **145632 KiB after 145000 records** | | | | | |
| batch 59 (147500 records) | 2500 | 14823 | 65.5 | 75.8 | 99.8 | 218.1 |
|   resident | **145744 KiB after 147500 records** | | | | | |
| batch 60 (150000 records) | 2500 | 15400 | 63.6 | 72.2 | 83.0 | 147.7 |
|   resident | **145744 KiB after 150000 records** | | | | | |
| batch 61 (152500 records) | 2500 | 15357 | 64.2 | 70.0 | 77.6 | 195.2 |
|   resident | **145760 KiB after 152500 records** | | | | | |
| batch 62 (155000 records) | 2500 | 14701 | 65.8 | 75.0 | 92.2 | 856.1 |
|   resident | **145760 KiB after 155000 records** | | | | | |
| batch 63 (157500 records) | 2500 | 15048 | 65.2 | 72.2 | 81.0 | 166.1 |
|   resident | **145936 KiB after 157500 records** | | | | | |
| batch 64 (160000 records) | 2500 | 14436 | 66.2 | 73.5 | 88.2 | 3088.9 |
|   resident | **146496 KiB after 160000 records** | | | | | |
| batch 65 (162500 records) | 2500 | 14681 | 66.3 | 75.0 | 99.4 | 252.8 |
|   resident | **146896 KiB after 162500 records** | | | | | |
| batch 66 (165000 records) | 2500 | 15390 | 64.2 | 70.8 | 78.5 | 142.8 |
|   resident | **146976 KiB after 165000 records** | | | | | |
| batch 67 (167500 records) | 2500 | 15676 | 63.5 | 69.4 | 77.8 | 131.6 |
|   resident | **147344 KiB after 167500 records** | | | | | |
| batch 68 (170000 records) | 2500 | 15623 | 62.8 | 69.1 | 79.3 | 677.2 |
|   resident | **147520 KiB after 170000 records** | | | | | |
| batch 69 (172500 records) | 2500 | 15696 | 63.2 | 69.5 | 76.1 | 123.7 |
|   resident | **147520 KiB after 172500 records** | | | | | |
| batch 70 (175000 records) | 2500 | 14989 | 65.9 | 72.3 | 79.5 | 124.3 |
|   resident | **147520 KiB after 175000 records** | | | | | |
| batch 71 (177500 records) | 2500 | 14787 | 66.0 | 73.9 | 90.3 | 206.2 |
|   resident | **147520 KiB after 177500 records** | | | | | |
| batch 72 (180000 records) | 2500 | 15398 | 64.4 | 70.4 | 77.4 | 109.6 |
|   resident | **147520 KiB after 180000 records** | | | | | |
| batch 73 (182500 records) | 2500 | 15913 | 62.0 | 67.8 | 75.1 | 148.1 |
|   resident | **147520 KiB after 182500 records** | | | | | |
| batch 74 (185000 records) | 2500 | 15672 | 62.5 | 69.0 | 79.1 | 855.2 |
|   resident | **147520 KiB after 185000 records** | | | | | |
| batch 75 (187500 records) | 2500 | 15966 | 61.8 | 68.3 | 74.2 | 138.0 |
|   resident | **147520 KiB after 187500 records** | | | | | |
| batch 76 (190000 records) | 2500 | 15404 | 63.5 | 70.8 | 85.3 | 198.6 |
|   resident | **147520 KiB after 190000 records** | | | | | |
| batch 77 (192500 records) | 2500 | 15297 | 64.0 | 71.2 | 89.4 | 417.7 |
|   resident | **147520 KiB after 192500 records** | | | | | |
| batch 78 (195000 records) | 2500 | 15915 | 61.9 | 68.2 | 76.7 | 183.9 |
|   resident | **147520 KiB after 195000 records** | | | | | |
| batch 79 (197500 records) | 2500 | 15830 | 62.2 | 68.3 | 75.7 | 182.8 |
|   resident | **147520 KiB after 197500 records** | | | | | |
| batch 80 (200000 records) | 2500 | 15983 | 61.8 | 67.5 | 73.0 | 117.5 |
|   resident | **147520 KiB after 200000 records** | | | | | |
| batch 81 (202500 records) | 2500 | 15677 | 62.8 | 68.8 | 77.5 | 164.0 |
|   resident | **147520 KiB after 202500 records** | | | | | |
| batch 82 (205000 records) | 2500 | 15629 | 62.9 | 69.4 | 88.2 | 180.2 |
|   resident | **147520 KiB after 205000 records** | | | | | |
| batch 83 (207500 records) | 2500 | 15295 | 64.3 | 70.8 | 85.8 | 202.5 |
|   resident | **147520 KiB after 207500 records** | | | | | |
| batch 84 (210000 records) | 2500 | 15686 | 62.1 | 71.0 | 87.2 | 177.5 |
|   resident | **147520 KiB after 210000 records** | | | | | |
| batch 85 (212500 records) | 2500 | 15686 | 62.3 | 69.8 | 77.5 | 362.6 |
|   resident | **147520 KiB after 212500 records** | | | | | |
| batch 86 (215000 records) | 2500 | 15831 | 62.2 | 69.3 | 76.1 | 125.3 |
|   resident | **147520 KiB after 215000 records** | | | | | |
| batch 87 (217500 records) | 2500 | 15844 | 62.0 | 68.8 | 79.5 | 173.0 |
|   resident | **147520 KiB after 217500 records** | | | | | |
| batch 88 (220000 records) | 2500 | 15497 | 63.1 | 69.7 | 83.4 | 1276.6 |
|   resident | **147520 KiB after 220000 records** | | | | | |
| batch 89 (222500 records) | 2500 | 15204 | 64.7 | 71.7 | 84.2 | 250.2 |
|   resident | **147520 KiB after 222500 records** | | | | | |
| batch 90 (225000 records) | 2500 | 15657 | 62.5 | 70.2 | 84.5 | 196.4 |
|   resident | **147520 KiB after 225000 records** | | | | | |
| batch 91 (227500 records) | 2500 | 15797 | 62.1 | 69.7 | 78.4 | 184.1 |
|   resident | **147520 KiB after 227500 records** | | | | | |
| batch 92 (230000 records) | 2500 | 15830 | 62.0 | 69.5 | 77.3 | 138.8 |
|   resident | **147520 KiB after 230000 records** | | | | | |
| batch 93 (232500 records) | 2500 | 15814 | 62.2 | 69.0 | 78.9 | 171.2 |
|   resident | **147520 KiB after 232500 records** | | | | | |
| batch 94 (235000 records) | 2500 | 15623 | 63.3 | 69.5 | 77.6 | 129.9 |
|   resident | **147520 KiB after 235000 records** | | | | | |
| batch 95 (237500 records) | 2500 | 15122 | 64.7 | 72.4 | 92.8 | 178.9 |
|   resident | **147520 KiB after 237500 records** | | | | | |
| batch 96 (240000 records) | 2500 | 15644 | 63.0 | 69.8 | 77.2 | 131.4 |
|   resident | **147520 KiB after 240000 records** | | | | | |
| batch 97 (242500 records) | 2500 | 15907 | 62.1 | 68.7 | 74.8 | 105.2 |
|   resident | **147520 KiB after 242500 records** | | | | | |
| batch 98 (245000 records) | 2500 | 15932 | 62.0 | 67.7 | 73.4 | 109.6 |
|   resident | **147520 KiB after 245000 records** | | | | | |
| batch 99 (247500 records) | 2500 | 15627 | 63.0 | 69.7 | 76.7 | 190.2 |
|   resident | **147520 KiB after 247500 records** | | | | | |
| batch 100 (250000 records) | 2500 | 15562 | 63.3 | 70.0 | 78.2 | 165.7 |
|   resident | **147520 KiB after 250000 records** | | | | | |

## restore

a backup and the restore that replays it — the readiness row that has to be timed

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| backup | 1 | 913 | 1094.9 | 1094.9 | 1094.9 | 1094.9 |
| backup size | **2004 record(s), 187406 bytes** | | | | | |
| restore | 1 | 102 | 9780.2 | 9780.2 | 9780.2 | 9780.2 |
| restored | **2004 record(s), truncated: false** | | | | | |

## vector-index

the same read served by a graph, with the recall it buys against the exact scan

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| vector-index-build | 1 | 6 | 158984.7 | 158984.7 | 158984.7 | 158984.7 |
| vector-exact-scan | 100 | 265 | 3760.4 | 3854.0 | 4420.4 | 4429.7 |
| vector-graph-walk | 100 | 1224 | 806.2 | 859.2 | 990.8 | 999.0 |
| recall | **100.0% of the exact ten, over 1000 asked** | | | | | |

## vector

a nearest-neighbour read over a scan — the number an HNSW index has to beat

| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |
|---|---|---|---|---|---|---|
| vector-write | 2000 | 15936 | 61.0 | 72.0 | 84.3 | 105.9 |
| vector-nearest-scan | 100 | 256 | 3818.7 | 4167.7 | 4339.5 | 4354.2 |
