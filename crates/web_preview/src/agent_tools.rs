//! Browser tools for agents, served over MCP from inside Cue and registered as
//! a built-in context server, the same way as the database tools. They act on
//! the preview tab the user is looking at, not a separate headless browser.

use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use collections::HashMap;
use context_server::ContextServerCommand;
use context_server::{
    listener::{McpServer, McpServerTool, ToolResponse},
    types::{
        Implementation, InitializeResponse, ProtocolVersion, ServerCapabilities, ToolAnnotations,
        ToolResponseContent, ToolsCapabilities, requests,
    },
};
use futures::FutureExt as _;
use gpui::{
    AnyWindowHandle, AsyncApp, BorrowAppContext as _, Entity, EntityId, Global, Task, WeakEntity,
    Window,
};
use project::project_settings::ContextServerSettings;
use schemars::JsonSchema;
use serde::Deserialize;
use workspace::Workspace;

use crate::preview::WebPreview;

const SERVER_NAME: &str = "cue-browser";
const LOAD_TIMEOUT: Duration = Duration::from_secs(15);
/// Long enough for a just-activated tab to be painted and its view shown.
const REVEAL_DELAY: Duration = Duration::from_millis(150);

/// MCP servers per project, alive as long as the project.
#[derive(Default)]
struct BrowserToolServers(HashMap<EntityId, (McpServer, ToolContext)>);

impl Global for BrowserToolServers {}

#[derive(Clone, Default)]
struct ToolContext {
    workspaces: std::rc::Rc<std::cell::RefCell<Vec<(WeakEntity<Workspace>, AnyWindowHandle)>>>,
}

pub(crate) fn register(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let project = workspace.project().clone();
    if !project.read(cx).is_local() {
        // The agent of a remote project runs on the remote host, which cannot
        // reach this machine's socket.
        return;
    }
    let project_id = project.entity_id();
    let entry = (cx.entity().downgrade(), window.window_handle());
    if let Some(servers) = cx.try_global::<BrowserToolServers>()
        && let Some((_, context)) = servers.0.get(&project_id)
    {
        context.workspaces.borrow_mut().push(entry);
        return;
    }
    let context = ToolContext::default();
    context.workspaces.borrow_mut().push(entry);
    let context_servers = project.read(cx).context_server_store();
    cx.spawn(async move |_, cx| {
        let mut server = match McpServer::new(cx).await {
            Ok(server) => server,
            Err(error) => {
                log::error!("starting the browser MCP server: {error:#}");
                return;
            }
        };
        server.handle_request::<requests::Initialize>(|params, _| {
            Task::ready(Ok(InitializeResponse {
                protocol_version: ProtocolVersion(params.protocol_version.0),
                capabilities: ServerCapabilities {
                    tools: Some(ToolsCapabilities {
                        list_changed: Some(false),
                    }),
                    ..ServerCapabilities::default()
                },
                server_info: Implementation {
                    name: SERVER_NAME.to_string(),
                    title: Some("Cue web preview".to_string()),
                    version: "1".to_string(),
                    description: None,
                },
                meta: None,
            }))
        });
        server.add_tool(Navigate(context.clone()));
        server.add_tool(Click(context.clone()));
        server.add_tool(Type(context.clone()));
        server.add_tool(Evaluate(context.clone()));
        server.add_tool(Screenshot(context.clone()));
        server.add_tool(Console(context.clone()));
        let socket = server.socket_path().to_string_lossy().to_string();
        cx.update(|cx| {
            context_servers.update(cx, |store, cx| {
                store.set_builtin_server(
                    SERVER_NAME.into(),
                    Some(ContextServerSettings::Stdio {
                        enabled: true,
                        remote: false,
                        command: ContextServerCommand {
                            path: "/usr/bin/nc".into(),
                            args: vec!["-U".to_string(), socket],
                            env: None,
                            timeout: None,
                        },
                    }),
                    cx,
                );
            });
            cx.default_global::<BrowserToolServers>()
                .0
                .insert(project_id, (server, context));
            cx.observe_release(&project, move |_, cx| {
                if cx.has_global::<BrowserToolServers>() {
                    cx.update_global::<BrowserToolServers, _>(|servers, _| {
                        servers.0.remove(&project_id);
                    });
                }
            })
            .detach();
        });
    })
    .detach();
}

impl ToolContext {
    /// The first open preview and the window it lives in.
    fn find_preview(
        &self,
        cx: &AsyncApp,
    ) -> Option<(Entity<WebPreview>, WeakEntity<Workspace>, AnyWindowHandle)> {
        self.workspaces
            .borrow()
            .iter()
            .find_map(|(workspace, window)| {
                let preview = workspace.upgrade()?.read_with(cx, |workspace, cx| {
                    workspace.items_of_type::<WebPreview>(cx).next()
                })?;
                Some((preview, workspace.clone(), *window))
            })
    }

    fn any_workspace(&self) -> Result<(WeakEntity<Workspace>, AnyWindowHandle)> {
        self.workspaces
            .borrow()
            .iter()
            .find(|(workspace, _)| workspace.upgrade().is_some())
            .cloned()
            .context("no Cue window is open for this project")
    }

    /// The preview, brought to the front of its pane so the user watches what
    /// the agent does and so real clicks can land on it.
    async fn visible_preview(&self, cx: &mut AsyncApp) -> Result<Entity<WebPreview>> {
        let (preview, workspace, window) = self
            .find_preview(cx)
            .context("no web preview is open; call browser_navigate first")?;
        if preview.read_with(cx, |preview, _| preview.native().is_visible()) {
            return Ok(preview);
        }
        window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.activate_item(&preview, false, false, window, cx);
                })
            })
            .context("the Cue window was closed")??;
        cx.background_executor().timer(REVEAL_DELAY).await;
        Ok(preview)
    }
}

fn text(text: impl Into<String>) -> ToolResponse<()> {
    ToolResponse {
        content: vec![ToolResponseContent::Text { text: text.into() }],
        structured_content: (),
    }
}

fn annotations(title: &str, read_only: bool) -> ToolAnnotations {
    ToolAnnotations {
        title: Some(title.to_string()),
        read_only_hint: Some(read_only),
        destructive_hint: Some(false),
        idempotent_hint: Some(read_only),
        open_world_hint: Some(true),
    }
}

#[derive(Clone)]
struct Navigate(ToolContext);

/// Loads a URL in Cue's web preview tab (opening one if needed) and waits for the page's DOM to load. The user sees the same page.
#[derive(Deserialize, JsonSchema)]
struct NavigateInput {
    /// The URL; a bare host such as localhost:3000 gets http://.
    url: String,
}

impl McpServerTool for Navigate {
    type Input = NavigateInput;
    type Output = ();
    const NAME: &'static str = "browser_navigate";

    fn annotations(&self) -> ToolAnnotations {
        annotations("Open a URL in the preview", false)
    }

    async fn run(&self, input: Self::Input, cx: &mut AsyncApp) -> Result<ToolResponse<()>> {
        let loaded = match self.0.find_preview(cx) {
            Some((preview, _, window)) => window
                .update(cx, |_, window, cx| {
                    preview.update(cx, |preview, cx| {
                        let loaded = preview.next_load();
                        preview.navigate(&input.url, window, cx).map(|_| loaded)
                    })
                })
                .context("the Cue window was closed")??,
            None => {
                let (workspace, window) = self.0.any_workspace()?;
                window
                    .update(cx, |_, window, cx| {
                        workspace.update(cx, |workspace, cx| {
                            let preview = WebPreview::open(workspace, &input.url, window, cx)?;
                            anyhow::Ok(preview.update(cx, |preview, _| preview.next_load()))
                        })
                    })
                    .context("the Cue window was closed")???
            }
        };
        let timeout = cx.background_executor().timer(LOAD_TIMEOUT);
        futures::select_biased! {
            url = loaded.fuse() => match url {
                Ok(url) => Ok(text(format!("Loaded {url}"))),
                Err(_) => bail!("the preview closed before the page loaded"),
            },
            _ = timeout.fuse() => Ok(text(format!(
                "Navigation started but the page did not finish loading within {}s",
                LOAD_TIMEOUT.as_secs()
            ))),
        }
    }
}

#[derive(Clone)]
struct Click(ToolContext);

/// Clicks the first element matching a CSS selector in the web preview. The click is a real mouse event at the element's center, so the page treats it like the user's own (trusted, with hover, focus and default actions).
#[derive(Deserialize, JsonSchema)]
struct ClickInput {
    /// CSS selector, as for document.querySelector.
    selector: String,
}

impl McpServerTool for Click {
    type Input = ClickInput;
    type Output = ();
    const NAME: &'static str = "browser_click";

    fn annotations(&self) -> ToolAnnotations {
        annotations("Click an element in the preview", false)
    }

    async fn run(&self, input: Self::Input, cx: &mut AsyncApp) -> Result<ToolResponse<()>> {
        let preview = self.0.visible_preview(cx).await?;
        Ok(text(click_selector(&preview, &input.selector, cx).await?))
    }
}

/// Clicks the element's center with real mouse events and describes what was clicked.
async fn click_selector(
    preview: &Entity<WebPreview>,
    selector: &str,
    cx: &mut AsyncApp,
) -> Result<String> {
    let selector_json = serde_json::to_string(selector)?;
    let script = format!(
        "const element = document.querySelector({selector_json}); \
         if (!element) return JSON.stringify({{ error: 'no element matches' }}); \
         element.scrollIntoView({{ block: 'center', inline: 'center', behavior: 'instant' }}); \
         const rect = element.getBoundingClientRect(); \
         return JSON.stringify({{ x: rect.left + rect.width / 2, y: rect.top + rect.height / 2, \
           tag: element.tagName.toLowerCase(), text: (element.innerText || element.value || '').slice(0, 80) }});"
    );
    let result = preview
        .read_with(cx, |preview, _| preview.native().evaluate(&script))
        .await
        .map_err(|_| anyhow!("the preview closed"))??;
    #[derive(Deserialize)]
    struct Target {
        error: Option<String>,
        x: Option<f64>,
        y: Option<f64>,
        tag: Option<String>,
        text: Option<String>,
    }
    let target: Target = serde_json::from_str(&result)?;
    if let Some(error) = target.error {
        bail!("{error}: {selector}");
    }
    let (x, y) = target
        .x
        .zip(target.y)
        .context("the page returned no position")?;
    preview.update(cx, |preview, _| {
        preview.ignore_page_focus_briefly();
        preview.native().click(x, y)
    })?;
    Ok(format!(
        "Clicked <{}> \"{}\" at ({x:.0}, {y:.0})",
        target.tag.unwrap_or_default(),
        target.text.unwrap_or_default().trim()
    ))
}

#[derive(Clone)]
struct Type(ToolContext);

/// Clicks the element matching a CSS selector to focus it, then types text into it as real keyboard text input (trusted input events, so frameworks like React see it). Text is inserted at the cursor; it does not clear the field first.
#[derive(Deserialize, JsonSchema)]
struct TypeInput {
    /// CSS selector of the field to type into, as for document.querySelector.
    selector: String,
    text: String,
    /// Press Return after typing, e.g. to submit a form or search.
    #[serde(default)]
    submit: bool,
}

impl McpServerTool for Type {
    type Input = TypeInput;
    type Output = ();
    const NAME: &'static str = "browser_type";

    fn annotations(&self) -> ToolAnnotations {
        annotations("Type into the preview", false)
    }

    async fn run(&self, input: Self::Input, cx: &mut AsyncApp) -> Result<ToolResponse<()>> {
        let preview = self.0.visible_preview(cx).await?;
        let clicked = click_selector(&preview, &input.selector, cx).await?;
        preview.update(cx, |preview, _| {
            preview.ignore_page_focus_briefly();
            preview.native().type_text(&input.text, input.submit)
        })?;
        Ok(text(format!(
            "{clicked}, typed {} characters{}",
            input.text.chars().count(),
            if input.submit {
                " and pressed Return"
            } else {
                ""
            }
        )))
    }
}

#[derive(Clone)]
struct Evaluate(ToolContext);

/// Runs JavaScript in the web preview's page as the body of an async function: use `await` freely and `return` a value, which comes back JSON-encoded. Use it for anything the other tools don't cover: reading the DOM, choosing <select> options, hovering, waiting for an element or for data to load.
#[derive(Deserialize, JsonSchema)]
struct EvaluateInput {
    /// Function body, e.g. `await new Promise(r => setTimeout(r, 500)); return document.title;`
    script: String,
}

impl McpServerTool for Evaluate {
    type Input = EvaluateInput;
    type Output = ();
    const NAME: &'static str = "browser_evaluate";

    fn annotations(&self) -> ToolAnnotations {
        // It can do anything the signed-in page can, so clients that ask
        // before destructive tools should ask here.
        ToolAnnotations {
            destructive_hint: Some(true),
            ..annotations("Run JavaScript in the preview", false)
        }
    }

    async fn run(&self, input: Self::Input, cx: &mut AsyncApp) -> Result<ToolResponse<()>> {
        let preview = self.0.visible_preview(cx).await?;
        let body = evaluate_body(&input.script);
        let result = preview
            .read_with(cx, |preview, _| preview.native().evaluate(&body))
            .await
            .map_err(|_| anyhow!("the preview closed"))??;
        Ok(text(result))
    }
}

/// Wraps the agent's script so whatever it returns comes back as JSON text,
/// and a thrown error comes back as text rather than WebKit's opaque error.
fn evaluate_body(script: &str) -> String {
    format!(
        "try {{ const value = await (async () => {{ {script}\n }})(); \
         const json = JSON.stringify(value); return json === undefined ? 'undefined' : json; }} \
         catch (error) {{ return 'Error: ' + (error && error.stack || String(error)); }}"
    )
}

#[derive(Clone)]
struct Screenshot(ToolContext);

/// Captures the web preview exactly as the user sees it, as a PNG.
#[derive(Deserialize, JsonSchema)]
struct ScreenshotInput {}

impl McpServerTool for Screenshot {
    type Input = ScreenshotInput;
    type Output = ();
    const NAME: &'static str = "browser_screenshot";

    fn annotations(&self) -> ToolAnnotations {
        annotations("Screenshot the preview", true)
    }

    async fn run(&self, _: Self::Input, cx: &mut AsyncApp) -> Result<ToolResponse<()>> {
        let preview = self.0.visible_preview(cx).await?;
        let png = preview
            .read_with(cx, |preview, _| preview.native().snapshot())
            .await
            .map_err(|_| anyhow!("the preview closed"))??;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Image {
                data: base64::engine::general_purpose::STANDARD.encode(png),
                mime_type: "image/png".to_string(),
            }],
            structured_content: (),
        })
    }
}

#[derive(Clone)]
struct Console(ToolContext);

/// Returns the web preview's console output (console.log/info/warn/error/debug, uncaught errors and unhandled promise rejections) since the last call.
#[derive(Deserialize, JsonSchema)]
struct ConsoleInput {}

impl McpServerTool for Console {
    type Input = ConsoleInput;
    type Output = ();
    const NAME: &'static str = "browser_console";

    fn annotations(&self) -> ToolAnnotations {
        annotations("Read the preview's console", false)
    }

    async fn run(&self, _: Self::Input, cx: &mut AsyncApp) -> Result<ToolResponse<()>> {
        let (preview, _, _) = self
            .0
            .find_preview(cx)
            .context("no web preview is open; call browser_navigate first")?;
        let (entries, dropped) = preview.update(cx, |preview, _| preview.take_console());
        Ok(text(serde_json::to_string_pretty(&serde_json::json!({
            "entries": entries,
            "dropped_older_entries": dropped,
        }))?))
    }
}
