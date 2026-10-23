import "./styles/tokens.css";
import "./styles/settings.css";
import { mustGetElementById } from "./shared/dom";

interface PlannedSetting {
  readonly name: string;
  readonly detail: string;
}

// A placeholder until the real settings land (ROADMAP M6). Copy is Grape's
// own.
const PLANNED: readonly PlannedSetting[] = [
  { name: "Keys", detail: "choose which key opens each wheel." },
  {
    name: "Where files go",
    detail: "next to the original, or a folder you pick.",
  },
  { name: "Sounds", detail: "turn the soft tick on or off." },
  { name: "Launch at login", detail: "start Grape when you sign in." },
];

const list = mustGetElementById(document, "planned");
for (const setting of PLANNED) {
  const item = document.createElement("li");
  const name = document.createElement("strong");
  name.textContent = setting.name;
  item.append(name, `: ${setting.detail}`);
  list.append(item);
}
