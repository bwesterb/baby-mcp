# Reference data (not in this repository)

baby-mcp compares weight with the WHO Child Growth Standards
(weight-for-age). The table is not distributed with this repository; put it
on disk yourself:

    curl -fsSL -o who-weight-for-age.tsv \
      https://raw.githubusercontent.com/WorldHealthOrganization/anthro/master/data-raw/growthstandards/weianthro.txt

It is the WHO's table of LMS parameters per day of age (0–1826 days),
tab-separated with a header: `sex` (1 = male, 2 = female), `age` (days),
`l`, `m` (median, kg), `s`. It comes from the WHO's own R package
`anthro` (<https://github.com/WorldHealthOrganization/anthro>); the
standards are published at <https://www.who.int/tools/child-growth-standards>.

The server reads it from `growth_table` in the config, by default
`who-weight-for-age.tsv` in the state directory. Without it, baby-mcp runs
without percentiles. Tests use `data/who-weight-for-age.tsv` when present
and skip the WHO-specific checks otherwise.

## First-month weight change

`newborn-weight-change.tsv`: percentiles (P5, P10, P25, P50, P75, P90, P95)
of the percentage weight change since birth, per delivery mode, for days
0–30 in steps of 0.1. Columns: `delivery` (`vaginal` or `cesarean`), `day`,
`p5` … `p95`. Read from Figure 3 of Paul IM et al., *Weight Change Nomograms
for the First Month After Birth*, Pediatrics 2016;138(6):e20162625
(© American Academy of Pediatrics), which publishes the curves only as a
figure. Calibrated on the figure's grid lines; it agrees with the numbers in
the paper's text (C-section median −7.1% at 68 h, back at birth weight at day
10; 24% / 8% not back by day 14 / 21; vaginal 14% / 5%). Accuracy about
±0.3 percentage points, less in the first two days, where the curves overlap
in the figure. For personal use; it is not distributed with this repository.

The server reads it from `weight_change_table` in the config, by default
`newborn-weight-change.tsv` in the state directory; `baby.delivery` selects
the curve. Without it, baby-mcp runs without weight-change percentiles.
