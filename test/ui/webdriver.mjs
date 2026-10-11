// A WebDriver client, as small as the UI test needs: one session, scripts,
// and clicks and keys on the elements a script returns. It speaks to
// tauri-driver (https://tauri.app/develop/tests/webdriver/), which starts the
// app and hands the session to the platform's driver for its webview.

const ELEMENT = "element-6066-11e4-a52e-4f735466cecf";

export class Session {
  #base;
  #id;

  constructor(base, id) {
    this.#base = base;
    this.#id = id;
  }

  /** Starts `application` and a session on its window. */
  static async start(base, application) {
    const response = await fetch(`${base}/session`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ capabilities: { alwaysMatch: { "tauri:options": { application } } } }),
    });
    const body = await response.json();
    if (!response.ok || !body.value?.sessionId) throw new Error(`no WebDriver session: ${JSON.stringify(body).slice(0, 600)}`);
    return new Session(base, body.value.sessionId);
  }

  async #call(method, path, body) {
    const response = await fetch(`${this.#base}/session/${this.#id}${path}`, {
      method,
      headers: { "Content-Type": "application/json" },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    const answer = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(`${method} ${path}: ${JSON.stringify(answer.value ?? answer).slice(0, 600)}`);
    return answer.value;
  }

  /** Runs `script` in the page (its `arguments` are `args`) and returns what it returns. */
  run(script, ...args) {
    return this.#call("POST", "/execute/sync", { script, args });
  }

  /** A real click on an element a script returned. */
  click(element) {
    return this.#call("POST", `/element/${element[ELEMENT]}/click`, {});
  }

  /** Replaces what a field holds by typing `text` into it. */
  async type(element, text) {
    await this.#call("POST", `/element/${element[ELEMENT]}/clear`, {});
    if (text !== "") await this.#call("POST", `/element/${element[ELEMENT]}/value`, { text });
  }

  /** Ends the session, which closes the app as a kill does: its exit handler does not run. */
  async end() {
    await fetch(`${this.#base}/session/${this.#id}`, { method: "DELETE" }).catch(() => {});
  }
}
