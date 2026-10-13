import { Plugin } from "@opencode/plugin/tui"
import { createSignal, For, onCleanup, Show } from "solid-js"

/// Quota in the sidebar: the shim answers `/api/rpc/antex.quota/panel` with lines
/// already laid out (`src/quota.rs` in the antex repo). Built in here, rather than
/// installed as an OpenCode plugin, because a plugin's data half runs inside
/// OpenCode's own server — antex's server is the Rust shim and runs no JS.
///
/// The call goes through `client.rpc.call`: it is the generated client's only
/// "any id, any method" door, so this patch needs no generated-code change.
type Panel = { readonly lines: readonly string[]; readonly active: number; readonly updatedAt: number }

const REFRESH_MS = 60_000

function sessionIDOf(event: unknown) {
  const data = (event as { data?: { sessionID?: unknown } } | undefined)?.data
  return typeof data?.sessionID === "string" ? data.sessionID : undefined
}

export function SidebarQuota(props: { context: Plugin.Context; sessionID: string }) {
  const [panel, setPanel] = createSignal<Panel>()
  const [loaded, setLoaded] = createSignal(false)
  const [open, setOpen] = createSignal(true)
  const theme = props.context.theme
  const lines = () => panel()?.lines ?? []
  const expandable = () => lines().length > 2

  let inFlight = false
  const load = async () => {
    if (inFlight) return
    inFlight = true
    try {
      const response = await props.context.client.rpc.call({
        rpcID: "antex.quota",
        method: "panel",
        input: { sessionID: props.sessionID },
      })
      const data = (response?.output as { data?: Panel } | undefined)?.data
      // A failed refresh keeps the previous lines on screen rather than blanking the panel.
      if (data) setPanel(data)
    } catch (error) {
      console.warn(`[antex] quota panel unavailable: ${error}`)
    } finally {
      inFlight = false
      setLoaded(true)
    }
  }
  void load()
  const interval = setInterval(() => void load(), REFRESH_MS)
  const stop = props.context.data.on("session.step.ended", (event) => {
    if (sessionIDOf(event) === props.sessionID) void load()
  })
  onCleanup(() => {
    clearInterval(interval)
    stop()
  })

  return (
    <box flexDirection="column">
      <box
        flexDirection="row"
        gap={1}
        onMouseDown={() => {
          if (expandable()) setOpen((value) => !value)
        }}
      >
        <Show when={expandable()}>
          <text fg={theme.text.base}>{open() ? "▼" : "▶"}</text>
        </Show>
        <text fg={theme.text.base}>
          <b>Quota</b>
          <Show when={!open() && (panel()?.active ?? 0) > 0}>
            <span style={{ fg: theme.text.muted }}> ({panel()?.active} active)</span>
          </Show>
        </text>
      </box>
      <Show
        when={loaded()}
        fallback={
          <text fg={theme.text.muted} wrapMode="none">
            …
          </text>
        }
      >
        <Show
          when={lines().length > 0}
          fallback={
            <text fg={theme.text.muted} wrapMode="none">
              No quota data available
            </text>
          }
        >
          <Show when={!expandable() || open()}>
            <For each={lines()}>
              {(line) =>
                // Provider headers ("[DeepSeek]") read as headings, like the token rows.
                line.startsWith("[") ? (
                  <text fg={theme.text.base} wrapMode="none">
                    <b>{line}</b>
                  </text>
                ) : (
                  <text fg={theme.text.muted} wrapMode="none">
                    {line || " "}
                  </text>
                )
              }
            </For>
          </Show>
        </Show>
      </Show>
    </box>
  )
}

export default Plugin.define({
  id: "antex.sidebar.quota",
  setup(context) {
    context.ui.slot({
      append: "sidebar.content",
      render: (props) => <SidebarQuota context={context} sessionID={props.sessionID} />,
    })
  },
})
