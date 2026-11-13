//! Finds running IIS Express processes and attaches dotnet-debugger to them.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, bail};
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, Task,
    WeakEntity, Window,
};
use picker::{Picker, PickerDelegate};
use sysinfo::{ProcessRefreshKind, RefreshKind, System, UpdateKind};
use task::SharedTaskContext;
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::{debug, same_path};

pub(crate) const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// IIS Express loads the CLR a few hundred milliseconds after it starts, so a process that has
/// been running for this long without one never will.
const ATTACH_RUNTIME_TIMEOUT: Duration = Duration::from_secs(5);
const NO_IIS_EXPRESS: &str = "No IIS Express is running. Start the site with `dotnet: run` \
    first, or start it under the debugger with `dotnet: debug`.";

/// A running `iisexpress.exe`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct IisExpressProcess {
    pub(crate) pid: u32,
    /// `None` when Zed can't read it, as for an elevated process.
    pub(crate) exe: Option<PathBuf>,
    /// The command line, program included.
    pub(crate) command: Vec<String>,
    /// Seconds since the Unix epoch.
    pub(crate) start_time: u64,
    /// The 32-bit IIS Express, which dotnet-debugger can't debug.
    pub(crate) is_32_bit: bool,
}

impl IisExpressProcess {
    /// The `/site:`, `/siteid:` or `/path:` argument, which names the site the process serves.
    pub(crate) fn site_argument(&self) -> Option<&str> {
        self.command.iter().map(String::as_str).find(|argument| {
            ["/site:", "/siteid:", "/path:"].iter().any(|prefix| {
                argument
                    .get(..prefix.len())
                    .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
            })
        })
    }

    /// The name of the site, or of the folder that a `/path:` site serves.
    pub(crate) fn site_name(&self) -> String {
        let Some((option, value)) = self
            .site_argument()
            .and_then(|argument| argument.split_once(':'))
        else {
            return format!("IIS Express process {}", self.pid);
        };
        if option.eq_ignore_ascii_case("/path") {
            let folder = value.trim_end_matches(['\\', '/']);
            folder
                .rsplit(['\\', '/'])
                .next()
                .unwrap_or(folder)
                .to_owned()
        } else if option.eq_ignore_ascii_case("/siteid") {
            format!("site {value}")
        } else {
            value.to_owned()
        }
    }
}

/// The running IIS Express processes, newest first.
pub(crate) fn running_processes() -> Vec<IisExpressProcess> {
    let refresh_kind = RefreshKind::nothing().with_processes(
        ProcessRefreshKind::nothing()
            .without_tasks()
            .with_exe(UpdateKind::Always)
            .with_cmd(UpdateKind::Always),
    );
    let program_files_x86 = std::env::var_os("ProgramFiles(x86)").map(PathBuf::from);
    let mut processes = System::new_with_specifics(refresh_kind)
        .processes()
        .values()
        .filter(|process| {
            process
                .name()
                .to_string_lossy()
                .eq_ignore_ascii_case("iisexpress.exe")
        })
        .map(|process| {
            let exe = process.exe().map(Path::to_path_buf);
            IisExpressProcess {
                pid: process.pid().as_u32(),
                is_32_bit: exe
                    .as_deref()
                    .is_some_and(|exe| is_32_bit_iis_express(exe, program_files_x86.as_deref())),
                exe,
                command: process
                    .cmd()
                    .iter()
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect(),
                start_time: process.start_time(),
            }
        })
        .collect::<Vec<_>>();
    sort_newest_first(&mut processes);
    processes
}

fn sort_newest_first(processes: &mut [IisExpressProcess]) {
    processes.sort_by(|left, right| {
        right
            .start_time
            .cmp(&left.start_time)
            .then(right.pid.cmp(&left.pid))
    });
}

/// IIS Express installs its 32-bit build in the 32-bit Program Files folder, next to the 64-bit
/// one in `C:\Program Files`.
fn is_32_bit_iis_express(exe: &Path, program_files_x86: Option<&Path>) -> bool {
    program_files_x86.is_some_and(|program_files_x86| {
        same_path(
            exe,
            &program_files_x86.join("IIS Express").join("iisexpress.exe"),
        )
    })
}

/// The newest of `processes` that `iis_express` started for the site after `started_at`.
pub(crate) fn find_site_process(
    processes: &[IisExpressProcess],
    iis_express: &Path,
    site_argument: &str,
    started_at: u64,
) -> Option<u32> {
    processes
        .iter()
        // Process start times have a resolution of one second.
        .filter(|process| process.start_time + 1 >= started_at)
        .filter(|process| {
            process
                .exe
                .as_deref()
                .is_some_and(|exe| same_path(exe, iis_express))
        })
        .filter(|process| {
            process
                .command
                .iter()
                .any(|argument| argument.eq_ignore_ascii_case(site_argument))
        })
        .max_by_key(|process| process.start_time)
        .map(|process| process.pid)
}

/// Whether the process has loaded `clr.dll`, the .NET Framework 4 runtime.
pub(crate) async fn has_clr(process_id: u32) -> bool {
    has_module(process_id, "clr.dll").await
}

/// Whether the process has loaded a .NET runtime: the .NET Framework's, or the .NET (Core) one of
/// an ASP.NET Core site that IIS Express hosts in process.
async fn has_dotnet_runtime(process_id: u32) -> bool {
    has_clr(process_id).await || has_module(process_id, "coreclr.dll").await
}

async fn has_module(process_id: u32, module: &str) -> bool {
    let output = util::command::new_command("tasklist")
        .args(["/M", module, "/FI"])
        .arg(format!("PID eq {process_id}"))
        .args(["/FO", "CSV", "/NH"])
        .output()
        .await;
    // Only matching processes are listed (`"iisexpress.exe","1234","clr.dll"`); otherwise
    // tasklist prints a localized message.
    output.is_ok_and(|output| {
        String::from_utf8_lossy(&output.stdout).contains(&format!("\"{process_id}\""))
    })
}

/// `dotnet: attach to IIS Express`: attaches dotnet-debugger to the running IIS Express, after
/// asking which one when several are running.
pub(crate) fn attach_to_iis_express(
    _: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    cx.spawn_in(window, async move |workspace, cx| {
        let mut processes = cx.background_spawn(async { running_processes() }).await;
        workspace.update_in(cx, |workspace, window, cx| {
            if processes.len() > 1 {
                IisExpressPicker::toggle(processes, workspace, window, cx);
            } else if let Some(process) = processes.pop() {
                attach(process, workspace, window, cx);
            } else {
                workspace.show_error(anyhow!(NO_IIS_EXPRESS), cx);
            }
        })
    })
    .detach_and_log_err(cx);
}

fn attach(
    process: IisExpressProcess,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Err(error) = check_debuggable(&process) {
        workspace.show_error(error, cx);
        return;
    }
    let label = attach_label(&process);
    cx.spawn_in(window, async move |workspace, cx| {
        let waiting_since = Instant::now();
        while !has_dotnet_runtime(process.pid).await {
            if waiting_since.elapsed() > ATTACH_RUNTIME_TIMEOUT {
                let error = anyhow!(
                    "IIS Express (process {}) hasn't loaded the .NET runtime, so there's nothing \
                     for the debugger to attach to",
                    process.pid
                );
                return workspace.update(cx, |workspace, cx| workspace.show_error(error, cx));
            }
            cx.background_executor().timer(PROCESS_POLL_INTERVAL).await;
        }
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.start_debug_session(
                debug::attach_scenario(label, process.pid),
                SharedTaskContext::default(),
                None,
                None,
                window,
                cx,
            );
        })
    })
    .detach_and_log_err(cx);
}

fn check_debuggable(process: &IisExpressProcess) -> Result<()> {
    if process.is_32_bit {
        bail!(
            "{} runs in the 32-bit IIS Express (process {}), and dotnet-debugger only debugs \
             64-bit processes. Set Use64BitIISExpress to true in the project, build it for AnyCPU \
             or x64, and start it again with `dotnet: run`.",
            process.site_name(),
            process.pid
        );
    }
    Ok(())
}

fn attach_label(process: &IisExpressProcess) -> String {
    format!("Attach to {} (IIS Express)", process.site_name())
}

/// The process ID and bitness come first, because a `/path:` argument is often too long to show
/// in full.
fn process_details(process: &IisExpressProcess) -> String {
    let mut details = format!("process {}", process.pid);
    if process.is_32_bit {
        details.push_str(" · 32-bit, can't be debugged");
    }
    if let Some(site_argument) = process.site_argument() {
        details.push_str(" · ");
        details.push_str(site_argument);
    }
    details
}

struct IisExpressPicker {
    picker: Entity<Picker<IisExpressPickerDelegate>>,
}

impl IisExpressPicker {
    fn toggle(
        processes: Vec<IisExpressProcess>,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let workspace_handle = workspace.weak_handle();
        workspace.toggle_modal(window, cx, move |window, cx| {
            let delegate =
                IisExpressPickerDelegate::new(cx.entity().downgrade(), processes, workspace_handle);
            let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
            Self { picker }
        });
    }
}

impl Render for IisExpressPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(34.)).child(self.picker.clone())
    }
}

impl Focusable for IisExpressPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for IisExpressPicker {}
impl ModalView for IisExpressPicker {}

struct IisExpressPickerDelegate {
    picker: WeakEntity<IisExpressPicker>,
    workspace: WeakEntity<Workspace>,
    processes: Vec<IisExpressProcess>,
    candidates: Vec<StringMatchCandidate>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl IisExpressPickerDelegate {
    fn new(
        picker: WeakEntity<IisExpressPicker>,
        processes: Vec<IisExpressProcess>,
        workspace: WeakEntity<Workspace>,
    ) -> Self {
        let candidates = processes
            .iter()
            .enumerate()
            .map(|(index, process)| StringMatchCandidate::new(index, &process.site_name()))
            .collect();
        Self {
            picker,
            workspace,
            processes,
            candidates,
            matches: Vec::new(),
            selected_index: 0,
        }
    }
}

impl PickerDelegate for IisExpressPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "IIS Express picker"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Attach the debugger to IIS Express…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        index: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = index;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let background = cx.background_executor().clone();
        let candidates = self.candidates.clone();
        cx.spawn_in(window, async move |this, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .enumerate()
                    .map(|(index, candidate)| StringMatch {
                        candidate_id: index,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    100,
                    &Default::default(),
                    background,
                )
                .await
            };
            this.update(cx, |this, cx| {
                this.delegate.matches = matches;
                this.delegate.selected_index = 0;
                cx.notify();
            })
            .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(process) = self
            .matches
            .get(self.selected_index)
            .and_then(|string_match| self.processes.get(string_match.candidate_id))
            .cloned()
        else {
            return;
        };
        self.workspace
            .update(cx, |workspace, cx| attach(process, workspace, window, cx))
            .log_err();
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.picker
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let string_match = self.matches.get(index)?;
        let process = self.processes.get(string_match.candidate_id)?;
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex()
                        .child(HighlightedLabel::new(
                            string_match.string.clone(),
                            string_match.positions.clone(),
                        ))
                        .child(
                            Label::new(process_details(process))
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const IIS_EXPRESS: &str = "C:\\Program Files\\IIS Express\\iisexpress.exe";
    const IIS_EXPRESS_X86: &str = "C:\\Program Files (x86)\\IIS Express\\iisexpress.exe";

    fn process(pid: u32, exe: &str, arguments: &[&str], start_time: u64) -> IisExpressProcess {
        IisExpressProcess {
            pid,
            exe: Some(PathBuf::from(exe)),
            command: std::iter::once(exe)
                .chain(arguments.iter().copied())
                .map(str::to_owned)
                .collect(),
            start_time,
            is_32_bit: false,
        }
    }

    #[test]
    fn names_the_site() {
        let site = process(
            10,
            IIS_EXPRESS,
            &[
                "/config:C:\\src\\Shop\\.vs\\Shop\\config\\applicationhost.config",
                "/site:Shop.Web",
                "/apppool:Clr4IntegratedAppPool",
            ],
            100,
        );
        assert_eq!(site.site_argument(), Some("/site:Shop.Web"));
        assert_eq!(site.site_name(), "Shop.Web");
        assert_eq!(process_details(&site), "process 10 · /site:Shop.Web");
        let scenario = debug::attach_scenario(attach_label(&site), site.pid);
        assert_eq!(scenario.label.as_ref(), "Attach to Shop.Web (IIS Express)");
        assert_eq!(scenario.adapter.as_ref(), debug::DOTNET_DEBUGGER);
        assert_eq!(
            scenario.config,
            serde_json::json!({ "request": "attach", "processId": 10 })
        );

        let folder = process(
            11,
            IIS_EXPRESS,
            &["/PATH:C:\\src\\Legacy Web\\", "/port:8080"],
            100,
        );
        assert_eq!(folder.site_argument(), Some("/PATH:C:\\src\\Legacy Web\\"));
        assert_eq!(folder.site_name(), "Legacy Web");

        let site_id = process(12, IIS_EXPRESS, &["/siteid:2"], 100);
        assert_eq!(site_id.site_name(), "site 2");

        let default_site = process(13, IIS_EXPRESS, &["/systray:false"], 100);
        assert_eq!(default_site.site_argument(), None);
        assert_eq!(default_site.site_name(), "IIS Express process 13");
        assert_eq!(process_details(&default_site), "process 13");
    }

    #[test]
    fn refuses_the_32_bit_iis_express() {
        let program_files_x86 = Path::new("C:\\Program Files (x86)");
        let iis_express_x86 = program_files_x86.join("IIS Express").join("iisexpress.exe");
        assert!(is_32_bit_iis_express(
            &iis_express_x86,
            Some(program_files_x86)
        ));
        assert!(!is_32_bit_iis_express(
            Path::new(IIS_EXPRESS),
            Some(program_files_x86)
        ));
        assert!(!is_32_bit_iis_express(&iis_express_x86, None));

        let mut site = process(20, IIS_EXPRESS_X86, &["/site:Legacy"], 100);
        check_debuggable(&site).unwrap();
        site.is_32_bit = true;
        let error = check_debuggable(&site).unwrap_err().to_string();
        assert!(
            error.starts_with(
                "Legacy runs in the 32-bit IIS Express (process 20), and dotnet-debugger only \
                 debugs 64-bit processes."
            ),
            "{error}"
        );
        assert_eq!(
            process_details(&site),
            "process 20 · 32-bit, can't be debugged · /site:Legacy"
        );
    }

    #[test]
    fn finds_the_process_started_for_a_site() {
        let mut processes = vec![
            // Started before the debug session.
            process(1, IIS_EXPRESS, &["/site:Shop"], 90),
            process(2, IIS_EXPRESS, &["/site:Shop"], 100),
            process(3, IIS_EXPRESS, &["/site:Blog"], 101),
            process(4, IIS_EXPRESS_X86, &["/site:Shop"], 102),
            IisExpressProcess {
                exe: None,
                ..process(5, IIS_EXPRESS, &["/site:Shop"], 103)
            },
        ];
        sort_newest_first(&mut processes);
        assert_eq!(
            processes
                .iter()
                .map(|process| process.pid)
                .collect::<Vec<_>>(),
            vec![5, 4, 3, 2, 1]
        );

        let iis_express = Path::new(IIS_EXPRESS);
        assert_eq!(
            find_site_process(&processes, iis_express, "/site:shop", 100),
            Some(2)
        );
        // Start times are in whole seconds, so a process may seem to start a second early.
        assert_eq!(
            find_site_process(&processes, iis_express, "/site:Shop", 91),
            Some(2)
        );
        assert_eq!(
            find_site_process(&processes, iis_express, "/site:Shop", 85),
            Some(2)
        );
        assert_eq!(
            find_site_process(&processes, iis_express, "/site:Blog", 100),
            Some(3)
        );
        assert_eq!(
            find_site_process(&processes, iis_express, "/site:Shop", 102),
            None
        );
        assert_eq!(
            find_site_process(&processes, iis_express, "/site:Admin", 0),
            None
        );
    }
}
