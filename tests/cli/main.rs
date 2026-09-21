//! End-to-end behaviour of the binary, in an isolated home directory.

mod harness;

use std::path::Path;

use harness::Sandbox;

#[test]
fn a_fresh_installation_lists_nothing_and_says_where_the_configuration_goes() {
    Sandbox::new()
        .run(&["ls"])
        .success()
        .stdout_contains("no VMs are running")
        .stdout_contains("no images are configured")
        // Built from components: the layout is the same everywhere, but the
        // separator vitro prints is the host's.
        .stdout_contains(
            &Path::new(".config")
                .join("vitro")
                .join("config.toml")
                .display()
                .to_string(),
        );
}

#[test]
fn ls_json_is_valid_json_on_a_fresh_installation() {
    let sandbox = Sandbox::new();

    let run = sandbox.run(&["ls", "--json"]).success();

    let json = run.json();
    assert_eq!(json["vms"].as_array().unwrap().len(), 0);
    assert_eq!(json["golden"].as_array().unwrap().len(), 0);
}

#[test]
fn a_configured_image_whose_golden_is_absent_is_shown_as_missing() {
    let sandbox = Sandbox::new();
    sandbox.write_config(
        r#"
        [defaults]
        ssh_user = "dev"

        [images.linux]
        golden = "~/.local/share/vitro/golden/linux.qcow2"
        "#,
    );

    sandbox
        .run(&["ls"])
        .success()
        .stdout_contains("linux")
        .stdout_contains("missing");
}

#[test]
fn a_golden_image_that_exists_is_shown_with_its_size() {
    let sandbox = Sandbox::new();
    sandbox.write_file(".local/share/vitro/golden/linux.qcow2", &[0u8; 2048]);
    sandbox.write_config(
        r#"
        [defaults]
        ssh_user = "dev"

        [images.linux]
        golden = "~/.local/share/vitro/golden/linux.qcow2"
        "#,
    );

    sandbox
        .run(&["ls"])
        .success()
        .stdout_contains("2K")
        .stdout_contains("qemu");
}

#[test]
fn the_configuration_file_is_named_when_it_cannot_be_parsed() {
    let sandbox = Sandbox::new();
    sandbox.write_config("this is not toml");

    sandbox
        .run(&["ls"])
        .failure()
        .stderr_contains("config.toml");
}

#[test]
fn a_misspelled_setting_is_reported_with_the_image_it_is_in() {
    let sandbox = Sandbox::new();
    sandbox.write_config(
        r#"
        [images.linux]
        golden = "/srv/linux.qcow2"
        ssh_user = "dev"
        momery = "8G"
        "#,
    );

    sandbox.run(&["ls"]).failure().stderr_contains("momery");
}

#[test]
fn a_missing_required_setting_names_the_image_and_the_setting() {
    let sandbox = Sandbox::new();
    sandbox.write_config(
        r#"
        [images.linux]
        ssh_user = "dev"
        "#,
    );

    sandbox
        .run(&["ls"])
        .failure()
        .stderr_contains("linux")
        .stderr_contains("golden");
}

#[test]
fn xdg_variables_move_the_whole_layout() {
    let sandbox = Sandbox::new();
    let elsewhere = sandbox.home().join("elsewhere");
    std::fs::create_dir_all(elsewhere.join("vitro")).unwrap();
    std::fs::write(
        elsewhere.join("vitro/config.toml"),
        "[images.linux]\ngolden = \"/srv/linux.qcow2\"\nssh_user = \"dev\"\n",
    )
    .unwrap();

    sandbox
        .run_with_env(&["ls"], &[("XDG_CONFIG_HOME", elsewhere.to_str().unwrap())])
        .success()
        .stdout_contains("linux");
}

#[test]
fn a_screenshot_of_a_vm_without_a_monitor_says_why_rather_than_writing_nothing() {
    let sandbox = Sandbox::new();
    sandbox.write_vm_record("linux-7f3a2c", std::process::id(), 53422);

    sandbox
        .run(&["screenshot", "linux-7f3a2c"])
        .failure()
        .stderr_contains("monitor socket");
}

#[test]
fn no_arguments_reports_status_and_what_to_do_next() {
    Sandbox::new()
        .run(&[])
        .success()
        .stdout_contains("NEXT")
        .stdout_contains("config.toml");
}

#[test]
fn doctor_reports_on_every_prerequisite_even_the_ones_that_pass() {
    // The point of `doctor` is answering "which qemu did it pick up?", so a
    // silent success would defeat it.
    Sandbox::new()
        .run(&["doctor"])
        .stdout_contains("ssh key")
        .stdout_contains("state directory");
}

#[test]
fn asking_about_a_vm_that_does_not_exist_says_so_and_points_at_ls() {
    for command in [
        vec!["port", "nope"],
        vec!["inspect", "nope"],
        vec!["promote", "nope"],
    ] {
        Sandbox::new()
            .run(&command)
            .failure()
            .stderr_contains("no VM is called");
    }
}

#[test]
fn an_ambiguous_prefix_lists_the_candidates_instead_of_guessing() {
    let sandbox = Sandbox::new();
    sandbox.write_vm_record("linux-aaaaaa", 4_000_000, 53422);
    sandbox.write_vm_record("linux-bbbbbb", 4_000_001, 53423);

    sandbox
        .run(&["port", "linux-"])
        .failure()
        .stderr_contains("linux-aaaaaa, linux-bbbbbb");
}

#[test]
fn a_unique_prefix_is_enough_to_name_a_vm() {
    let sandbox = Sandbox::new();
    sandbox.write_vm_record("linux-7f3a2c", std::process::id(), 53422);

    sandbox
        .run(&["port", "linux-7f"])
        .success()
        .stdout_is("53422\n");
}

#[test]
fn a_dead_vms_port_is_refused_rather_than_pointing_at_whatever_took_it_over() {
    let sandbox = Sandbox::new();
    sandbox.write_vm_record("linux-7f3a2c", 4_000_000, 53422);

    sandbox
        .run(&["port", "linux-7f3a2c"])
        .failure()
        .stderr_contains("released");
}

#[test]
fn ssh_config_publishes_an_alias_that_hides_the_port() {
    let sandbox = Sandbox::new();
    sandbox.write_vm_record("linux-7f3a2c", std::process::id(), 53422);

    sandbox
        .run(&["ssh-config"])
        .success()
        .stdout_contains("Host vitro-linux-7f3a2c")
        .stdout_contains("Port 53422");
}

#[test]
fn writing_the_include_file_says_where_it_went() {
    let sandbox = Sandbox::new();

    sandbox
        .run(&["ssh-config", "--write"])
        .success()
        .stdout_contains("ssh_config");

    let written = std::fs::read_to_string(sandbox.home().join(".config/vitro/ssh_config")).unwrap();
    assert!(written.contains("Written by vitro"), "{written}");
}

#[test]
fn a_port_that_is_not_a_port_is_refused_before_anything_connects() {
    let sandbox = Sandbox::new();
    sandbox.write_vm_record("linux-7f3a2c", 4_000_000, 53422);

    sandbox
        .run(&["forward", "linux-7f3a2c", "http"])
        .failure()
        .stderr_contains("port number");
}

#[test]
fn running_an_image_that_is_not_configured_names_the_ones_that_are() {
    let sandbox = Sandbox::new();
    sandbox.write_config(
        r#"
        [images.linux]
        golden = "/srv/linux.qcow2"
        ssh_user = "dev"
        "#,
    );

    sandbox
        .run(&["run", "linuz"])
        .failure()
        .stderr_contains("known images are linux");
}

#[cfg(unix)]
#[test]
fn run_then_ls_then_destroy_leaves_nothing_behind() {
    let sandbox = Sandbox::new();
    let golden = sandbox.write_file(".local/share/vitro/golden/linux.qcow2", &[0u8; 1024]);
    sandbox.write_config(&format!(
        r#"
        [images.linux]
        golden = "{}"
        ssh_user = "dev"
        firmware = "/dev/null"
        "#,
        golden.display()
    ));
    sandbox.fake_qemu("qemu-system-aarch64");
    sandbox.fake_qemu("qemu-system-x86_64");
    sandbox.fake_bin("qemu-img", 0);
    sandbox.fake_bin("ssh", 0);
    sandbox.fake_bin("scp", 0);

    let started = sandbox
        .run(&["run", "linux"])
        .success()
        .stdout_contains("dev@127.0.0.1");
    let name = started
        .stdout
        .split_whitespace()
        .next()
        .expect("the name is printed first")
        .to_string();

    sandbox
        .run(&["ls"])
        .success()
        .stdout_contains(&name)
        .stdout_contains("running");

    sandbox
        .run(&["destroy", &name])
        .success()
        .stdout_contains("stopped");

    sandbox
        .run(&["ls"])
        .success()
        .stdout_contains("no VMs are running");
    assert!(
        !sandbox.state_dir().join("vms").join(&name).exists(),
        "the VM directory should be gone"
    );
}

#[cfg(unix)]
#[test]
fn a_run_that_cannot_reach_the_guest_cleans_up_after_itself() {
    let sandbox = Sandbox::new();
    let golden = sandbox.write_file(".local/share/vitro/golden/linux.qcow2", &[0u8; 1024]);
    sandbox.write_config(&format!(
        r#"
        [defaults]
        boot_timeout = "1s"

        [images.linux]
        golden = "{}"
        ssh_user = "dev"
        firmware = "/dev/null"
        "#,
        golden.display()
    ));
    sandbox.fake_qemu("qemu-system-aarch64");
    sandbox.fake_qemu("qemu-system-x86_64");
    sandbox.fake_bin("qemu-img", 0);
    // Never answers, so the boot timeout is what ends the wait.
    sandbox.fake_bin("ssh", 255);
    sandbox.fake_bin("scp", 0);

    sandbox
        .run(&["run", "linux"])
        .failure()
        .stderr_contains("no SSH answer");

    sandbox
        .run(&["ls"])
        .success()
        .stdout_contains("no VMs are running");
    let vms = sandbox.state_dir().join("vms");
    let leftovers: Vec<_> = std::fs::read_dir(&vms)
        .map(|d| d.filter_map(Result::ok).map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "left behind {leftovers:?}");
}

#[cfg(unix)]
#[test]
fn the_qemu_command_line_is_the_one_the_experiments_settled_on() {
    let sandbox = Sandbox::new();
    let golden = sandbox.write_file(".local/share/vitro/golden/linux.qcow2", &[0u8; 1024]);
    sandbox.write_config(&format!(
        r#"
        [images.linux]
        golden = "{}"
        ssh_user = "dev"
        firmware = "/dev/null"
        cpus = 2
        memory = "4G"
        resolution = "1920x1080"
        "#,
        golden.display()
    ));
    sandbox.fake_qemu("qemu-system-aarch64");
    sandbox.fake_qemu("qemu-system-x86_64");
    sandbox.fake_bin("qemu-img", 0);
    sandbox.fake_bin("ssh", 0);
    sandbox.fake_bin("scp", 0);

    sandbox.run(&["run", "linux"]).success();

    let calls = [
        sandbox.calls("qemu-system-aarch64"),
        sandbox.calls("qemu-system-x86_64"),
    ]
    .concat();
    let line = calls.first().expect("qemu should have been run").clone();

    assert!(line.contains("-smp 2"), "{line}");
    assert!(line.contains("-m 4096"), "{line}");
    assert!(
        line.contains("virtio-gpu-pci,xres=1920,yres=1080"),
        "{line}"
    );
    assert!(line.contains("-display none"), "{line}");
    assert!(line.contains("-daemonize"), "{line}");
    assert!(line.contains("hostfwd=tcp:127.0.0.1:"), "{line}");
    assert!(!line.contains("media=cdrom"), "{line}");
}

#[cfg(unix)]
#[test]
fn exec_passes_the_guest_exit_code_through() {
    let sandbox = Sandbox::new();
    let golden = sandbox.write_file(".local/share/vitro/golden/linux.qcow2", &[0u8; 1024]);
    sandbox.write_config(&format!(
        r#"
        [images.linux]
        golden = "{}"
        ssh_user = "dev"
        firmware = "/dev/null"
        "#,
        golden.display()
    ));
    sandbox.fake_qemu("qemu-system-aarch64");
    sandbox.fake_qemu("qemu-system-x86_64");
    sandbox.fake_bin("qemu-img", 0);
    sandbox.fake_bin("ssh", 0);
    sandbox.fake_bin("scp", 0);

    let started = sandbox.run(&["run", "linux"]).success();
    let name = started
        .stdout
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();

    // Replace ssh so the guest "command" fails with a distinctive code.
    sandbox.fake_bin("ssh", 42);
    let run = sandbox.run(&["exec", &name, "false"]);
    assert_eq!(run.code, 42, "stderr: {}", run.stderr);

    sandbox.fake_bin("ssh", 0);
    sandbox.run(&["destroy", &name]).success();
}

#[test]
fn an_unknown_subcommand_is_a_usage_error() {
    Sandbox::new().run(&["summon"]).usage_error();
}

#[test]
fn help_describes_the_whole_tool_not_just_the_finished_parts() {
    let run = Sandbox::new().run(&["--help"]).success();

    for command in [
        "build",
        "run",
        "exec",
        "promote",
        "screenshot",
        "launch",
        "doctor",
    ] {
        assert!(
            run.stdout.contains(command),
            "--help should mention {command}\n{}",
            run.stdout
        );
    }
}

#[cfg(unix)]
#[test]
fn the_fake_binary_scaffolding_shadows_path_and_records_arguments() {
    // Proves the harness itself works, so the first command that shells out
    // starts from a foundation that has been exercised.
    let sandbox = Sandbox::new();
    sandbox.fake_bin("qemu-img", 0);

    let status = std::process::Command::new("qemu-img")
        .args(["create", "-f", "qcow2", "overlay.qcow2"])
        .env("PATH", {
            let mut path = sandbox.home().join("bin").into_os_string();
            path.push(":");
            path.push(std::env::var_os("PATH").unwrap_or_default());
            path
        })
        .status()
        .expect("the fake should be runnable");

    assert!(status.success());
    assert_eq!(sandbox.calls("qemu-img"), ["create -f qcow2 overlay.qcow2"]);
}
