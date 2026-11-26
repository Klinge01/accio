import { listen } from "@tauri-apps/api/event";

void listen<string>("drop://message", ({ payload }) => {
  document.querySelector("body > div")!.textContent = payload;
});
