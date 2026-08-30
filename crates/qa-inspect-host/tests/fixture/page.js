const button = document.createElement("button");
button.setAttribute("aria-label", "Press me");
button.textContent = "Press me";
let presses = 0;
button.addEventListener("click", () => {
  presses += 1;
  button.setAttribute("aria-label", `Pressed ${presses}`);
});
document.getElementById("root").appendChild(button);
const input = document.createElement("input");
input.setAttribute("aria-label", "Fixture value");
input.value = "before";
document.getElementById("root").appendChild(input);
globalThis.__mounted = true;
