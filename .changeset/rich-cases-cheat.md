---
"@biomejs/biome": patch
---

Added the new nursery rule [`useThisForClassMethods`](https://biomejs.dev/linter/rules/use-this-for-class-methods/), based on ESLint's `class-methods-use-this`.

The rule now reports instance methods, getters, setters, and function-valued instance fields that do not use `this`, and `biome migrate eslint` preserves the supported `exceptMethods`, `ignoreOverrideMethods`, and `ignoreClassesWithImplements` options.
