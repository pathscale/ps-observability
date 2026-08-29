const button = document.createElement("button");
button.setAttribute("aria-label", "Press me");
button.textContent = "Press me";
document.getElementById("root").appendChild(button);
globalThis.__mounted = true;
