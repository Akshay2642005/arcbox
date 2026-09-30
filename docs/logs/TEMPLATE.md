# YYYY-MM-DD — <what was done or asked, as a sentence>

- Type: change | experiment | investigation | incident
- Area: <crates / subsystem>
- Commits / PRs: <hashes, or "none (not merged)">
- Outcome: <ADR NNNN | fixed | rejected | open>
- Probes / scripts: <path in tests/bench or e2e target>
- Host / guest: <hardware, macOS, load; guest shape and kernel facts that
  affect the numbers>

## Question or problem

One paragraph. Numbers that motivated the work, with how they were taken.

## Method

What was run, against what, how many times. Enough for someone to rerun
it; refer to checked-in scripts rather than pasting them.

## Results

Tables. Same units and same conditions per row; note the load next to the
numbers when it varied.

## Findings

Numbered. Each one a claim the results support, with the mechanism when it
is known and "unknown" when it is not.

## Decisions taken / open

What changed because of this (link the ADR or commit), and what is still
unanswered.
