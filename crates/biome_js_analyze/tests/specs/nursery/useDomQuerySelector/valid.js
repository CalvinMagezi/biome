/* should not generate diagnostics */
document.querySelector("#foo");
document.querySelectorAll(".foo");

function shadowed(document) {
	document.getElementById("foo");
}

const document = customApi;
document.getElementById("foo");

customApi.getElementById("foo");
element.getElementsByClassName("foo");
document?.getElementById("foo");
document.getElementById?.("foo");
document.getElementById();
document.getElementById("foo", "bar");
