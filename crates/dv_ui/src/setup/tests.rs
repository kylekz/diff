//! The setup screen (PLAN §12, DESIGN §4.3) against a scratch home and the user `Path`'s
//! stand-in file in the test's data dir: the first-run screen in place of the columns until
//! Continue (`Enter`) writes the marker, Settings opening on `Ctrl ,` and closing on `Esc`, a
//! skill's Install and the out-of-date notice with its Update and dismiss.

use gpui::TestAppContext;
use pretty_assertions::assert_eq;

use crate::ActiveServices as _;
use crate::services::setup::ONBOARDING_FILE;
use crate::test_support::{open_shell, open_shell_with, settle};
use dv_cli::checks::State;

fn skill_state(cx: &mut gpui::VisualTestContext, id: &str) -> Option<State> {
    cx.update(|_, cx| {
        cx.services()
            .setup
            .read(cx)
            .environments()
            .first()
            .and_then(|here| here.env.check(id).map(|check| check.state))
    })
}

#[gpui::test]
fn the_first_run_screen_shows_until_continue_writes_the_marker(cx: &mut TestAppContext) {
    let (dir, shell, cx) = open_shell_with(cx, true);
    settle(cx, &shell, "this machine's checks", |shell, cx| {
        shell.first_run_open(cx) && !cx.services().setup.read(cx).environments().is_empty()
    });
    assert!(!dir.path().join(ONBOARDING_FILE).exists());
    // The first-run screen has the keyboard: Enter continues.
    cx.simulate_keystrokes("enter");
    settle(cx, &shell, "the marker", |shell, cx| {
        !shell.first_run_open(cx)
    });
    let marker = std::fs::read_to_string(dir.path().join(ONBOARDING_FILE)).unwrap();
    assert!(marker.contains("\"how\":\"continue\""), "{marker}");
    // The first implementation's marker is another file, left alone.
    assert!(!dir.path().join("setup.json").exists());
}

#[gpui::test]
fn without_the_first_run_settings_shows_the_same_checks(cx: &mut TestAppContext) {
    let (dir, shell, cx) = open_shell(cx);
    assert!(cx.update(|_, cx| !shell.read(cx).first_run_open(cx)));
    cx.simulate_keystrokes("secondary-,");
    settle(cx, &shell, "settings", |shell, _| {
        shell.settings().is_some()
    });
    assert_eq!(skill_state(cx, "skill-claude-code"), Some(State::Skipped));

    // Claude Code set up in the home: Check again finds its skill missing, Install puts it there.
    std::fs::create_dir_all(dir.path().join("home").join(".claude")).unwrap();
    let setup = cx.update(|_, cx| cx.services().setup.clone());
    setup.update(cx, |setup, cx| setup.check_again(cx));
    settle(cx, &shell, "the check", |_, cx| {
        cx.services().setup.read(cx).is_settled()
    });
    assert_eq!(skill_state(cx, "skill-claude-code"), Some(State::Missing));
    // This machine by its name: "Windows", "Linux", "macOS", or the distro dv runs inside.
    let here = cx.update(|_, cx| {
        cx.services().setup.read(cx).environments()[0]
            .env
            .name
            .clone()
    });
    setup.update(cx, |setup, cx| setup.apply(&here, "skill-claude-code", cx));
    settle(cx, &shell, "the install", |_, cx| {
        cx.services().setup.read(cx).is_settled()
    });
    assert_eq!(skill_state(cx, "skill-claude-code"), Some(State::Ok));

    cx.simulate_keystrokes("escape");
    settle(cx, &shell, "settings closed", |shell, _| {
        shell.settings().is_none()
    });
}

#[gpui::test]
fn an_out_of_date_skill_is_noticed_once_and_updated(cx: &mut TestAppContext) {
    let (dir, shell, cx) = open_shell(cx);
    let skill = dir
        .path()
        .join("home")
        .join(".claude")
        .join("skills")
        .join("dv-review")
        .join("SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(&skill, "an older dv's skill").unwrap();
    let setup = cx.update(|_, cx| cx.services().setup.clone());
    setup.update(cx, |setup, cx| setup.check_again(cx));
    settle(cx, &shell, "the check", |_, cx| {
        cx.services().setup.read(cx).notice().is_some()
    });
    let (here, kind, notice) = cx.update(|_, cx| {
        let setup = cx.services().setup.read(cx);
        let env = &setup.environments()[0].env;
        (env.name.clone(), env.kind, setup.notice().unwrap())
    });
    // This machine is "on Windows" or "on Linux", but "in Ubuntu" when dv runs inside WSL.
    let place = if kind == dv_cli::checks::Kind::Wsl {
        "in"
    } else {
        "on"
    };
    assert_eq!(
        notice.text,
        format!("dv-review skill out of date {place} {here}")
    );
    assert_eq!(notice.action, "Update");

    // Dismissed, it doesn't come back for the same skill this session.
    setup.update(cx, |setup, cx| setup.dismiss_notice(cx));
    assert!(cx.update(|_, cx| cx.services().setup.read(cx).notice().is_none()));
    setup.update(cx, |setup, cx| setup.check_again(cx));
    settle(cx, &shell, "the check", |_, cx| {
        cx.services().setup.read(cx).is_settled()
    });
    assert!(cx.update(|_, cx| cx.services().setup.read(cx).notice().is_none()));

    // Its fix updates it.
    setup.update(cx, |setup, cx| setup.apply(&here, "skill-claude-code", cx));
    settle(cx, &shell, "the update", |_, cx| {
        cx.services().setup.read(cx).is_settled()
    });
    assert_eq!(skill_state(cx, "skill-claude-code"), Some(State::Ok));
    assert_eq!(
        std::fs::read_to_string(&skill).unwrap(),
        dv_cli::skill::SKILL_MD
    );
}

#[gpui::test]
fn a_review_opening_behind_the_first_run_screen_leaves_it_the_keyboard(cx: &mut TestAppContext) {
    use crate::services::reviews::OpenRequest;
    use crate::test_support::{fixture, register};
    use dv_core::{Location, Target};

    let (_dir, shell, cx) = open_shell_with(cx, true);
    let repo = fixture();
    register(&repo, cx);
    // What `dv open <path>` asked for opens as ever, behind the screen.
    let location = Location::Local(repo.path().to_path_buf());
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell
                .open(location, OpenRequest::Target(Target::Working), window, cx)
                .detach()
        })
    });
    settle(cx, &shell, "the review", |shell, _| {
        shell.session().is_some()
    });
    assert!(cx.update(|_, cx| shell.read(cx).first_run_open(cx)));
    // Enter is still the first-run screen's: Continue.
    cx.simulate_keystrokes("enter");
    settle(cx, &shell, "the first run's end", |shell, cx| {
        !shell.first_run_open(cx)
    });
}

#[gpui::test]
fn a_distro_without_the_skill_is_noticed(cx: &mut TestAppContext) {
    use dv_cli::checks::{Check, Environment, Fix, Kind};

    let (_dir, shell, cx) = open_shell(cx);
    settle(cx, &shell, "this machine's skills", |_, cx| {
        !cx.services().setup.read(cx).environments().is_empty()
    });
    let setup = cx.update(|_, cx| cx.services().setup.clone());
    setup.update(cx, |setup, cx| {
        setup.insert_distro_for_test(
            Environment {
                name: "Ubuntu".to_string(),
                kind: Kind::Wsl,
                running: true,
                note: None,
                checks: vec![Check {
                    id: "skill-claude-code".to_string(),
                    title: "dv-review skill for Claude Code".to_string(),
                    state: State::Missing,
                    detail: "not installed: /home/k/.claude/skills/dv-review/SKILL.md".to_string(),
                    fix: Some(Fix {
                        action: Some("Install".to_string()),
                        command: Some("dv setup agents".to_string()),
                    }),
                }],
            },
            cx,
        )
    });
    let notice = cx.update(|_, cx| cx.services().setup.read(cx).notice().unwrap());
    assert_eq!(notice.text, "dv-review skill not installed in Ubuntu");
    assert_eq!(notice.action, "Install");
    assert_eq!(notice.environment, "Ubuntu");
}

#[gpui::test]
fn codex_and_cursor_share_one_install(cx: &mut TestAppContext) {
    let (dir, shell, cx) = open_shell(cx);
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::create_dir_all(home.join(".cursor")).unwrap();
    let setup = cx.update(|_, cx| cx.services().setup.clone());
    setup.update(cx, |setup, cx| setup.check_again(cx));
    settle(cx, &shell, "the check", |_, cx| {
        cx.services().setup.read(cx).is_settled()
    });
    assert_eq!(skill_state(cx, "skill-cursor"), Some(State::Missing));
    let here = cx.update(|_, cx| {
        cx.services().setup.read(cx).environments()[0]
            .env
            .name
            .clone()
    });
    // Both buttons at once: one write, no failure on either row.
    setup.update(cx, |setup, cx| {
        setup.apply(&here, "skill-codex", cx);
        setup.apply(&here, "skill-cursor", cx);
    });
    settle(cx, &shell, "the install", |_, cx| {
        cx.services().setup.read(cx).is_settled()
    });
    assert_eq!(skill_state(cx, "skill-codex"), Some(State::Ok));
    assert_eq!(skill_state(cx, "skill-cursor"), Some(State::Ok));
    cx.update(|_, cx| {
        let setup = cx.services().setup.read(cx);
        assert_eq!(setup.failure(&here, "skill-codex"), None);
        assert_eq!(setup.failure(&here, "skill-cursor"), None);
    });
}

#[gpui::test]
fn settings_appearance_changes_the_mode_and_the_material(cx: &mut TestAppContext) {
    use crate::services::Mode;
    use crate::test_support::bounds;
    use crate::theme::{ActiveTheme as _, Appearance, Material};
    use gpui_appearance::MaterialSupport;

    let (_dir, shell, cx) = open_shell(cx);
    cx.simulate_keystrokes("secondary-,");
    settle(cx, &shell, "settings", |shell, _| {
        shell.settings().is_some()
    });
    let click = |name: &'static str, cx: &mut gpui::VisualTestContext| {
        let at = bounds(cx, name).unwrap_or_else(|| panic!("{name} is drawn"));
        cx.simulate_click(at.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    };
    let chosen = |cx: &mut gpui::VisualTestContext| {
        cx.update(|_, cx| {
            let settings = cx.services().settings.read(cx);
            (settings.mode(), settings.material())
        })
    };

    // The platform's own translucent materials (Vibrancy alone on macOS).
    let (first, first_name, second_name) = if cfg!(target_os = "macos") {
        (
            Material::Vibrancy,
            "settings-material-vibrancy",
            "settings-material-vibrancy",
        )
    } else {
        (
            Material::Acrylic,
            "settings-material-acrylic",
            "settings-material-mica",
        )
    };
    click("settings-mode-light", cx);
    click(first_name, cx);
    assert_eq!(chosen(cx), (Mode::Light, first));
    cx.update(|_, cx| {
        let theme = cx.dv_theme();
        assert_eq!(
            (theme.appearance, theme.material),
            (Appearance::Light, first)
        );
    });

    // A material the platform can't show is shown Opaque, keeping the setting, and takes no
    // click.
    cx.update(|_, cx| gpui_appearance::set_support(MaterialSupport::OPAQUE_ONLY, cx));
    assert_eq!(chosen(cx), (Mode::Light, first));
    assert_eq!(cx.update(|_, cx| cx.dv_theme().material), Material::Opaque);
    click("settings-material-opaque", cx);
    assert_eq!(chosen(cx), (Mode::Light, Material::Opaque));
    click(second_name, cx);
    assert_eq!(chosen(cx), (Mode::Light, Material::Opaque));
}
