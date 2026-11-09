//! What the views' tests share: a main window with the shell over real services in a temporary
//! data dir, fixture repos registered with `Locations`, and a wait that lets real time pass for
//! background git work.

use std::time::{Duration, Instant};

use dv_core::Location;
use dv_test_support::FixtureRepo;
use gpui::{AppContext as _, Bounds, Entity, Pixels, TestAppContext, VisualTestContext, px, size};
use gpui_component::Root;

use crate::ActiveServices as _;
use crate::services::ServicesConfig;
use crate::shell::Shell;
use crate::system::SystemShell;
use gpui_appearance::MaterialSupport;

/// A 1440x900 main window around a [`Shell`], over services in a temporary data dir, with the
/// system seam scripted (nothing reaches the real clipboard or opens a picker).
pub fn open_shell(
    cx: &mut TestAppContext,
) -> (tempfile::TempDir, Entity<Shell>, &mut VisualTestContext) {
    open_shell_with(cx, false)
}

/// [`open_shell`], the first-run screen allowed (`first_run`) on its fresh data dir. The setup
/// checks run against a scratch home in the data dir (`home/`) and a stand-in user `Path`
/// (`user-path.txt`), never the machine's.
pub fn open_shell_with(
    cx: &mut TestAppContext,
    first_run: bool,
) -> (tempfile::TempDir, Entity<Shell>, &mut VisualTestContext) {
    cx.executor().allow_parking();
    let dir = tempfile::tempdir().expect("a temporary data dir");
    let data_dir = dir.path().to_path_buf();
    cx.update(|cx| {
        crate::init(cx).expect("dv_ui initialises");
        crate::system::install(SystemShell::scripted(), cx);
        crate::services::init(
            ServicesConfig {
                setup: crate::services::SetupConfig::sandboxed(
                    data_dir.join("home"),
                    data_dir.join("user-path.txt"),
                    first_run,
                ),
                data_dir,
                wsl: crate::services::WslAccess::scripted(Vec::new()),
            },
            cx,
        );
        crate::appearance::init(MaterialSupport::ALL, cx);
    });
    let mut shell = None;
    let window = cx.open_window(size(px(1440.), px(900.)), |window, cx| {
        let view = cx.new(|cx| Shell::new(window, cx));
        shell = Some(view.clone());
        Root::new(view, window, cx)
    });
    let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
    cx.run_until_parked();
    (dir, shell.expect("the window built a shell"), cx)
}

/// Has every backend opened for `repo` use its fixture environment.
pub fn register(repo: &FixtureRepo, cx: &mut VisualTestContext) {
    let locations = cx.update(|_, cx| cx.services().locations.clone());
    locations.update(cx, |locations, _| {
        locations.register_env(repo.path().to_path_buf(), repo.env_overlay())
    });
}

/// Adds `repo` to the known repositories (as `dv open` would, without the switcher) and waits
/// for its row.
pub fn add_repo(repo: &FixtureRepo, shell: &Entity<Shell>, cx: &mut VisualTestContext) {
    register(repo, cx);
    let reviews = cx.update(|_, cx| cx.services().reviews.clone());
    let added = reviews.update(cx, |reviews, cx| {
        reviews.add_repo(Location::Local(repo.path().to_path_buf()), cx)
    });
    cx.run_until_parked();
    settle(cx, shell, "the repo's row", |_, _| true);
    let added = cx.foreground_executor().block_test(added);
    assert!(added.is_ok(), "{added:?}");
}

/// Whether the services, the shell and its session have nothing left to do.
fn is_settled(shell: &Entity<Shell>, cx: &gpui::App) -> bool {
    let shell = shell.read(cx);
    cx.services().is_settled(cx)
        && shell.is_settled(cx)
        && shell.diff_pane().read(cx).is_settled()
        && shell.file_view().read(cx).is_settled()
        && shell
            .session()
            .is_none_or(|session| session.read(cx).is_settled())
        && shell
            .intel()
            .is_none_or(|intel| intel.read(cx).is_settled(cx))
}

/// Runs the app, letting real time pass for background git work and the watch, until
/// everything is settled and `done` holds.
pub fn settle(
    cx: &mut VisualTestContext,
    shell: &Entity<Shell>,
    what: &str,
    done: impl Fn(&Shell, &gpui::App) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        cx.executor().advance_clock(Duration::from_millis(400));
        cx.run_until_parked();
        let ready = cx.update(|_, cx| is_settled(shell, cx) && done(shell.read(cx), cx));
        if ready {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The bounds an element with `.debug_selector(|| name.into())` drew at, after a fresh frame.
pub fn bounds(cx: &mut VisualTestContext, name: &'static str) -> Option<Bounds<Pixels>> {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    cx.debug_bounds(name)
}

/// A repo with a modified, an untracked and a deleted file.
pub fn fixture() -> FixtureRepo {
    fill(FixtureRepo::new())
}

/// [`fixture`] built at `path`, so its name is the path's last component.
pub fn fixture_at(path: &std::path::Path) -> FixtureRepo {
    fill(FixtureRepo::persistent(path))
}

fn fill(repo: FixtureRepo) -> FixtureRepo {
    repo.write(
        "src/routes/users.ts",
        "export function list() {\n  return [];\n}\n",
    );
    repo.write("src/db.ts", "export const url = 'postgres://localhost';\n");
    repo.write("README.md", "# api-server\n");
    repo.commit("base");
    repo.write(
        "src/routes/users.ts",
        "export function list(limit: number) {\n  return [].slice(0, limit);\n}\n",
    );
    repo.write(
        "src/routes/users.test.ts",
        "import { list } from './users';\n",
    );
    repo.remove("README.md");
    repo
}
