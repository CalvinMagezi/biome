/* should generate diagnostics */
class ExceptMethods {
    foo() {
        return 1;
    }

    #bar() {
        return 2;
    }

    baz() {
        return 3;
    }

    ["foo"]() {
        return 4;
    }
}
