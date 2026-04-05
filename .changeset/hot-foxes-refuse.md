---
"@biomejs/biome": patch
---

Added the new nursery rule [`noLoopFunc`](https://biomejs.dev/linter/rules/no-loop-func/). Biome now warns when a function declared inside a loop captures outer variables that can change across iterations.
