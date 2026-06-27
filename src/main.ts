import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";

type Profile = {
  id: string;
  label: string;
  host: string;
  port: number;
  username: string;
};

type ConnectResponse = {
  sessionId: string;
};

type TerminalDataEvent = {
  sessionId: string;
  data: string;
};

type TerminalStatusEvent = {
  sessionId: string;
  state: "connecting" | "connected" | "disconnected" | "error";
  message: string;
};

type TerminalTab = {
  sessionId: string;
  title: string;
  state: TerminalStatusEvent["state"];
  terminal: Terminal;
  fitAddon: FitAddon;
  element: HTMLDivElement;
};

const STORAGE_KEY = "adit.profiles";

const starterProfiles: Profile[] = [
  {
    id: crypto.randomUUID(),
    label: "local-lab",
    host: "127.0.0.1",
    port: 22,
    username: "root",
  },
];

let profiles = loadProfiles();
let selectedProfileId: string | null = profiles[0]?.id ?? null;
let tabs: TerminalTab[] = [];
let activeSessionId: string | null = null;

const profileList = mustGet<HTMLDivElement>("profile-list");
const profileCount = mustGet<HTMLSpanElement>("profile-count");
const connectionForm = mustGet<HTMLFormElement>("connection-form");
const labelInput = mustGet<HTMLInputElement>("profile-label");
const hostInput = mustGet<HTMLInputElement>("host");
const portInput = mustGet<HTMLInputElement>("port");
const usernameInput = mustGet<HTMLInputElement>("username");
const passwordInput = mustGet<HTMLInputElement>("password");
const newProfileButton = mustGet<HTMLButtonElement>("new-profile-button");
const saveProfileButton = mustGet<HTMLButtonElement>("save-profile-button");
const tabList = mustGet<HTMLElement>("tab-list");
const terminalStage = mustGet<HTMLElement>("terminal-stage");
const emptyState = mustGet<HTMLElement>("empty-state");
const connectionState = mustGet<HTMLElement>("connection-state");

window.addEventListener("DOMContentLoaded", async () => {
  await wireTerminalEvents();
  bindControls();
  renderProfiles();
  hydrateSelectedProfile();
  renderTabs();
});

function bindControls() {
  newProfileButton.addEventListener("click", () => {
    selectedProfileId = null;
    labelInput.value = "";
    hostInput.value = "";
    portInput.value = "22";
    usernameInput.value = "";
    passwordInput.value = "";
    renderProfiles();
    hostInput.focus();
  });

  saveProfileButton.addEventListener("click", () => {
    const draft = readProfileDraft();
    if (!draft) {
      return;
    }

    const existingIndex = profiles.findIndex((profile) => profile.id === selectedProfileId);
    if (existingIndex >= 0) {
      profiles[existingIndex] = { ...draft, id: profiles[existingIndex].id };
      selectedProfileId = profiles[existingIndex].id;
    } else {
      const profile = { ...draft, id: crypto.randomUUID() };
      profiles = [profile, ...profiles];
      selectedProfileId = profile.id;
    }

    saveProfiles();
    renderProfiles();
  });

  connectionForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    await connectFromForm();
  });

  window.addEventListener("resize", () => {
    const active = getActiveTab();
    if (active) {
      fitAndResize(active);
    }
  });
}

async function wireTerminalEvents() {
  await listen<TerminalDataEvent>("terminal-data", (event) => {
    const tab = findTab(event.payload.sessionId);
    tab?.terminal.write(event.payload.data);
  });

  await listen<TerminalStatusEvent>("terminal-status", (event) => {
    const tab = findTab(event.payload.sessionId);
    if (!tab) {
      return;
    }

    tab.state = event.payload.state;
    connectionState.textContent = event.payload.message;

    if (event.payload.state === "error") {
      tab.terminal.writeln(`\r\n\x1b[31m${event.payload.message}\x1b[0m`);
    }

    if (event.payload.state === "disconnected") {
      tab.terminal.writeln("\r\n\x1b[33mDisconnected\x1b[0m");
    }

    renderTabs();
  });
}

async function connectFromForm() {
  const draft = readProfileDraft();
  if (!draft) {
    return;
  }

  const password = passwordInput.value;
  if (!password) {
    passwordInput.focus();
    return;
  }

  connectionState.textContent = "Connecting";

  try {
    const terminalSize = estimateTerminalSize();
    const response = await invoke<ConnectResponse>("ssh_connect", {
      request: {
        label: draft.label,
        host: draft.host,
        port: draft.port,
        username: draft.username,
        password,
        terminalCols: terminalSize.cols,
        terminalRows: terminalSize.rows,
      },
    });

    const tab = createTerminalTab(response.sessionId, draft.label || draft.host);
    tabs = [...tabs, tab];
    activeSessionId = response.sessionId;
    renderTabs();
    mountTerminal(tab);
    tab.terminal.writeln(`Connecting to ${draft.host}:${draft.port} as ${draft.username}`);
  } catch (error) {
    connectionState.textContent = stringifyError(error);
  }
}

function createTerminalTab(sessionId: string, title: string): TerminalTab {
  const terminal = new Terminal({
    allowProposedApi: false,
    cursorBlink: true,
    convertEol: true,
    fontFamily: "Cascadia Mono, Consolas, Menlo, monospace",
    fontSize: 13,
    lineHeight: 1.18,
    scrollback: 8000,
    theme: {
      background: "#111318",
      foreground: "#d8dee9",
      cursor: "#f5d76e",
      black: "#111318",
      red: "#e06c75",
      green: "#98c379",
      yellow: "#e5c07b",
      blue: "#61afef",
      magenta: "#c678dd",
      cyan: "#56b6c2",
      white: "#d8dee9",
      brightBlack: "#5c6370",
      brightRed: "#ef596f",
      brightGreen: "#89ca78",
      brightYellow: "#d19a66",
      brightBlue: "#61afef",
      brightMagenta: "#d55fde",
      brightCyan: "#2bbac5",
      brightWhite: "#ffffff",
    },
  });
  const fitAddon = new FitAddon();
  const element = document.createElement("div");
  element.className = "terminal-view";
  element.dataset.sessionId = sessionId;
  terminal.loadAddon(fitAddon);

  terminal.onData((data) => {
    invoke("ssh_write", { sessionId, data }).catch((error) => {
      terminal.writeln(`\r\n\x1b[31m${stringifyError(error)}\x1b[0m`);
    });
  });

  terminal.onResize(({ cols, rows }) => {
    invoke("ssh_resize", { sessionId, cols, rows }).catch(() => undefined);
  });

  return {
    sessionId,
    title,
    state: "connecting",
    terminal,
    fitAddon,
    element,
  };
}

function mountTerminal(tab: TerminalTab) {
  if (!terminalStage.contains(tab.element)) {
    terminalStage.appendChild(tab.element);
    tab.terminal.open(tab.element);
  }

  showActiveTerminal();
  requestAnimationFrame(() => {
    fitAndResize(tab);
    tab.terminal.focus();
  });
}

function fitAndResize(tab: TerminalTab) {
  tab.fitAddon.fit();
  invoke("ssh_resize", {
    sessionId: tab.sessionId,
    cols: tab.terminal.cols,
    rows: tab.terminal.rows,
  }).catch(() => undefined);
}

function renderProfiles() {
  profileCount.textContent = String(profiles.length);
  profileList.replaceChildren();

  profiles.forEach((profile) => {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "profile-item";
    button.dataset.active = String(profile.id === selectedProfileId);
    button.innerHTML = `
      <span class="profile-name"></span>
      <span class="profile-host"></span>
    `;
    button.querySelector(".profile-name")!.textContent = profile.label;
    button.querySelector(".profile-host")!.textContent = `${profile.username}@${profile.host}:${profile.port}`;
    button.addEventListener("click", () => {
      selectedProfileId = profile.id;
      hydrateSelectedProfile();
      renderProfiles();
    });
    profileList.appendChild(button);
  });
}

function hydrateSelectedProfile() {
  const selected = profiles.find((profile) => profile.id === selectedProfileId);
  if (!selected) {
    return;
  }

  labelInput.value = selected.label;
  hostInput.value = selected.host;
  portInput.value = String(selected.port);
  usernameInput.value = selected.username;
}

function renderTabs() {
  tabList.replaceChildren();

  tabs.forEach((tab) => {
    const tabButton = document.createElement("button");
    tabButton.type = "button";
    tabButton.className = "tab-button";
    tabButton.dataset.active = String(tab.sessionId === activeSessionId);
    tabButton.dataset.state = tab.state;
    tabButton.innerHTML = `
      <span class="tab-dot"></span>
      <span class="tab-title"></span>
      <span class="tab-close" title="Close">x</span>
    `;
    tabButton.querySelector(".tab-title")!.textContent = tab.title;
    tabButton.addEventListener("click", (event) => {
      const target = event.target as HTMLElement;
      if (target.classList.contains("tab-close")) {
        closeTab(tab.sessionId);
        return;
      }

      activeSessionId = tab.sessionId;
      showActiveTerminal();
      renderTabs();
      requestAnimationFrame(() => {
        fitAndResize(tab);
        tab.terminal.focus();
      });
    });
    tabList.appendChild(tabButton);
  });

  showActiveTerminal();
}

function showActiveTerminal() {
  const hasTabs = tabs.length > 0;
  emptyState.hidden = hasTabs;

  tabs.forEach((tab) => {
    tab.element.dataset.active = String(tab.sessionId === activeSessionId);
  });
}

function closeTab(sessionId: string) {
  const tab = findTab(sessionId);
  if (!tab) {
    return;
  }

  invoke("ssh_disconnect", { sessionId }).catch(() => undefined);
  tab.terminal.dispose();
  tab.element.remove();
  tabs = tabs.filter((item) => item.sessionId !== sessionId);

  if (activeSessionId === sessionId) {
    activeSessionId = tabs[tabs.length - 1]?.sessionId ?? null;
  }

  connectionState.textContent = activeSessionId ? "Active" : "Idle";
  renderTabs();
}

function readProfileDraft(): Omit<Profile, "id"> | null {
  const host = hostInput.value.trim();
  const username = usernameInput.value.trim();
  const port = Number(portInput.value.trim() || "22");
  const label = labelInput.value.trim() || host;

  if (!host || !username || !Number.isInteger(port) || port < 1 || port > 65535) {
    connectionForm.reportValidity();
    return null;
  }

  return {
    label,
    host,
    port,
    username,
  };
}

function estimateTerminalSize() {
  const width = Math.max(terminalStage.clientWidth - 32, 320);
  const height = Math.max(terminalStage.clientHeight - 32, 240);

  return {
    cols: Math.max(Math.floor(width / 8), 80),
    rows: Math.max(Math.floor(height / 16), 24),
  };
}

function loadProfiles(): Profile[] {
  const raw = localStorage.getItem(STORAGE_KEY);
  if (!raw) {
    return starterProfiles;
  }

  try {
    const parsed = JSON.parse(raw) as Profile[];
    if (Array.isArray(parsed) && parsed.length > 0) {
      return parsed;
    }
  } catch {
    return starterProfiles;
  }

  return starterProfiles;
}

function saveProfiles() {
  localStorage.setItem(STORAGE_KEY, JSON.stringify(profiles));
}

function getActiveTab() {
  return activeSessionId ? findTab(activeSessionId) : null;
}

function findTab(sessionId: string) {
  return tabs.find((tab) => tab.sessionId === sessionId);
}

function stringifyError(error: unknown) {
  if (error instanceof Error) {
    return error.message;
  }

  return String(error);
}

function mustGet<T extends HTMLElement>(id: string): T {
  const element = document.getElementById(id);
  if (!element) {
    throw new Error(`Missing required element: #${id}`);
  }

  return element as T;
}
