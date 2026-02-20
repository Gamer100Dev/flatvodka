use clap::{Parser, Subcommand};
use ini::Ini;
use nix::unistd::getuid;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use walkdir::WalkDir;

const USER_FLATPAK_BASE: &str = ".local/share/flatpak";
const FLATHUB_URL: &str = "https://dl.flathub.org/repo/";

#[derive(Parser)]
#[command(name = "flatvodka")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Run {
        app_id: String,
        #[arg(trailing_var_arg = true)]
        argv: Vec<String>,
        #[arg(long, default_value_t = true)]
        raw_sockets: bool,
    },
    Install {
        target: String,
    },
    List,
    Clean,
}

fn get_flatpak_dir() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME not set");
    PathBuf::from(home).join(USER_FLATPAK_BASE)
}

fn get_repo_dir() -> PathBuf {
    get_flatpak_dir().join("repo")
}

fn sys_mount(fstype: &str, source: &str, target: &str, ro: bool) {
    if !Path::new(target).exists() {
        let _ = fs::create_dir_all(target);
    }
    let mut cmd = Command::new("/sbin/mount");
    cmd.arg("-t").arg(fstype);
    if ro {
        cmd.arg("-o").arg("ro");
    }
    cmd.arg(source).arg(target);
    let _ = cmd.status();
}

fn mount_bind(source: &Path, target: &Path, ro: bool) {
    sys_mount("nullfs", source.to_str().unwrap(), target.to_str().unwrap(), ro);
}

fn find_ostree_binary() -> Option<String> {
    let ubuntu_path = "/compat/ubuntu/usr/bin/ostree";
    if Path::new(ubuntu_path).exists() {
        return Some(ubuntu_path.to_string());
    }
    let linux_path = "/compat/linux/usr/bin/ostree";
    if Path::new(linux_path).exists() {
        return Some(linux_path.to_string());
    }
    if Command::new("ostree").arg("--version").output().is_ok() {
        return Some("ostree".to_string());
    }
    None
}

fn ensure_repo_config_fixed(repo_dir: &Path) {
    let config_path = repo_dir.join("config");

    let fix_file = |path: &Path| {
        if path.exists() {
            if let Ok(mut conf) = Ini::load_from_file(path) {
                conf.with_section(Some("core")).set("summary-max-size", "268435456");
                let _ = conf.write_to_file(path);
            }
        }
    };

    fix_file(&config_path);

    if let Ok(home) = std::env::var("HOME") {
        let compat_config = PathBuf::from("/compat/ubuntu")
        .join(home.strip_prefix("/").unwrap_or(&home))
        .join(USER_FLATPAK_BASE)
        .join("repo/config");
        if compat_config.exists() && compat_config != config_path {
            fix_file(&compat_config);
        }
    }
}

fn install_logic(input: &str) {
    let repo_dir = get_repo_dir();
    let ostree_bin = find_ostree_binary().expect("❌ ostree binary not found. Please install it in /compat/ubuntu.");

    let (ref_id, remote_name, remote_url) = if input.ends_with(".flatpakref") {
        let path = Path::new(input);
        if !path.exists() { panic!("File not found: {}", input); }
        let conf = Ini::load_from_file(path).expect("Failed to parse .flatpakref");
        let sec = conf.section(Some("Flatpak Ref")).expect("Invalid flatpakref file");
        let name = sec.get("Name").expect("No Name in flatpakref");
        let url = sec.get("Url").expect("No Url in flatpakref");
        let branch = sec.get("Branch").unwrap_or("stable");
        (format!("app/{}/x86_64/{}", name, branch), "origin".to_string(), url.to_string())
    } else if input.contains('/') {
        if input.starts_with("runtime/") || input.starts_with("app/") {
            (input.to_string(), "flathub".to_string(), FLATHUB_URL.to_string())
        } else {
            (format!("runtime/{}", input), "flathub".to_string(), FLATHUB_URL.to_string())
        }
    } else {
        (format!("app/{}/x86_64/stable", input), "flathub".to_string(), FLATHUB_URL.to_string())
    };

    if !repo_dir.join("config").exists() {
        fs::create_dir_all(&repo_dir).unwrap();
        println!("🌱 Initializing new OSTree repo at {:?}", repo_dir);
        Command::new(&ostree_bin).args(&["init", "--mode=archive-z2", "--repo", repo_dir.to_str().unwrap()]).status().unwrap();
    }

    Command::new(&ostree_bin)
    .args(&["remote", "add", "--if-not-exists", "--no-gpg-verify", "--repo", &repo_dir.to_str().unwrap(), &remote_name, &remote_url])
    .status().unwrap();

    let config_key = format!("remote.{}.gpg-verify", &remote_name);
    Command::new(&ostree_bin)
    .args(&["config", "--repo", &repo_dir.to_str().unwrap(), &config_key, "false"])
    .status().unwrap();

    ensure_repo_config_fixed(&repo_dir);

    println!("⬇️  Pulling {} from {}...", ref_id, &remote_name);
    let status = Command::new(&ostree_bin)
    .args(&["pull", "--repo", &repo_dir.to_str().unwrap(), &remote_name, &ref_id])
    .status()
    .expect("ostree pull failed");
    if !status.success() {
        eprintln!("❌ Pull failed.");
        std::process::exit(1);
    }
    let output = Command::new(&ostree_bin)
    .args(&["rev-parse", "--repo", &repo_dir.to_str().unwrap(), &ref_id])
    .output().expect("rev-parse failed");
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let parts: Vec<&str> = ref_id.split('/').collect();
    let install_base = get_flatpak_dir().join(parts[0]).join(parts[1]).join(parts[2]).join(parts[3]);
    let commit_dir = install_base.join(&commit);
    let active_link = install_base.join("active");
    if !commit_dir.exists() || fs::read_dir(&commit_dir).ok().map_or(true, |mut d| d.next().is_none()) {
        println!("📦 Checking out...");
        if commit_dir.exists() {
            let _ = fs::remove_dir_all(&commit_dir);
        }
        fs::create_dir_all(commit_dir.parent().unwrap()).unwrap();
        let checkout_status = Command::new(&ostree_bin)
        .args(&[
            "checkout",
            "--repo", &repo_dir.to_str().unwrap(),
              "--user-mode",
              &commit,
              commit_dir.to_str().unwrap()
        ])
        .status()
        .expect("ostree checkout failed");
        if !checkout_status.success() {
            eprintln!("❌ Checkout failed");
            std::process::exit(1);
        }
    }
    if active_link.exists() { let _ = fs::remove_file(&active_link); }
    let _ = symlink(&commit, &active_link);
    println!("✅ Installed: {}", parts[1]);
    if parts[0] == "app" {
        let meta_1 = commit_dir.join("metadata");
        let meta_2 = commit_dir.join("files/metadata");
        let meta_path = if meta_1.exists() { meta_1 } else { meta_2 };
        if meta_path.exists() {
            if let Ok(conf) = Ini::load_from_file(&meta_path) {
                if let Some(sec) = conf.section(Some("Application")) {
                    if let Some(rt) = sec.get("runtime") {
                        println!("🔗 Found Dependency: {}", rt);
                        install_logic(rt);
                    }
                }
            }
        }
    }
}

fn run_app(app_id: &str, argv: Vec<String>, _raw_sockets: bool) {
    if !getuid().is_root() {
        eprintln!("⛔ Run requires root.");
        std::process::exit(1);
    }
    let user_name = std::env::var("SUDO_USER")
    .or_else(|_| std::env::var("USER"))
    .unwrap_or_else(|_| "root".to_string());
    let uid = std::env::var("SUDO_UID").unwrap_or("1000".to_string());
    println!("👤 Detected Host User: {} (UID: {})", user_name, uid);
    let base = get_flatpak_dir();
    let app_files = base.join("app").join(app_id).join("x86_64").join("stable").join("active").join("files");
    if !app_files.exists() {
        eprintln!("❌ App files not found: {:?}", app_files);
        std::process::exit(1);
    }
    let meta_path = app_files.parent().unwrap().join("metadata");
    let conf = Ini::load_from_file(&meta_path).expect("Metadata read failed");
    let app_sec = conf.section(Some("Application")).expect("Invalid metadata");
    let runtime_str = app_sec.get("runtime").expect("No runtime");
    let default_cmd = app_sec.get("command").unwrap_or("sh");
    let parts: Vec<&str> = runtime_str.split('/').collect();
    let rt_files = base.join("runtime").join(parts[0]).join(parts[1]).join(parts[2]).join("active").join("files");
    if !rt_files.exists() {
        eprintln!("❌ Runtime files not found: {:?}", rt_files);
        std::process::exit(1);
    }
    let jail_root = PathBuf::from(format!("/mnt/flatvodka_{}", app_id));
    let jname = format!("fv_{}", app_id.replace(".", "_"));
    if jail_root.exists() {
        println!("🧹 Cleaning up previous session...");
        let _ = Command::new("jail").arg("-r").arg(&jname).output();
        let _ = Command::new("umount").arg("-f").arg(jail_root.join("tmp/.X11-unix")).output();
        let _ = Command::new("umount").arg("-f").arg(jail_root.join("dev")).output();
        let _ = Command::new("umount").arg("-f").arg(jail_root.join("proc")).output();
        let _ = Command::new("umount").arg("-f").arg(jail_root.join("sys")).output();
        let _ = Command::new("umount").arg("-f").arg(jail_root.join("var/run/dbus")).output();
        let _ = Command::new("umount").arg("-f").arg(jail_root.join("run/host/fonts")).output();
        let atspi_target = jail_root.join(format!("var/run/xdg/{}/at-spi", user_name));
        let _ = Command::new("umount").arg("-f").arg(&atspi_target).output();
        if let Ok(wl) = std::env::var("WAYLAND_DISPLAY") {
            let wl_target = jail_root.join(format!("run/user/{}/{}", uid, wl));
            let _ = Command::new("umount").arg("-f").arg(&wl_target).output();
        }
        let _ = Command::new("umount").arg("-f").arg(&jail_root).output();
        let _ = fs::remove_dir_all(&jail_root);
    }
    fs::create_dir_all(&jail_root).unwrap();
    println!("💾 Creating tmpfs jail filesystem...");
    let tmpfs_status = Command::new("mount")
    .arg("-t").arg("tmpfs").arg("tmpfs").arg(&jail_root)
    .status().expect("Failed to mount tmpfs");
    if !tmpfs_status.success() {
        eprintln!("❌ Failed to create tmpfs");
        std::process::exit(1);
    }
    println!("📦 Copying runtime files...");
    let rt_files_path = rt_files.clone();
    let tar_rt = Command::new("tar")
    .current_dir(&rt_files_path)
    .arg("-cf").arg("-").arg(".")
    .stdout(Stdio::piped())
    .spawn()
    .and_then(|mut tar_create| {
        Command::new("tar")
        .current_dir(&jail_root)
        .arg("-xf").arg("-")
        .stdin(tar_create.stdout.take().unwrap())
        .status()
    });
    if tar_rt.is_err() || !tar_rt.unwrap().success() {
        eprintln!("❌ Failed to copy runtime files");
        std::process::exit(1);
    }
    println!("📦 Copying app files...");
    fs::create_dir_all(jail_root.join("app")).unwrap();
    let app_files_path = app_files.clone();
    let tar_app = Command::new("tar")
    .current_dir(&app_files_path)
    .arg("-cf").arg("-").arg(".")
    .stdout(Stdio::piped())
    .spawn()
    .and_then(|mut tar_create| {
        Command::new("tar")
        .current_dir(&jail_root.join("app"))
        .arg("-xf").arg("-")
        .stdin(tar_create.stdout.take().unwrap())
        .status()
    });
    if tar_app.is_err() || !tar_app.unwrap().success() {
        eprintln!("⚠️  Warning: App copy may have failed");
    }
    println!("🔗 Repairing filesystem paths...");
    let usr_dir = jail_root.join("usr");
    if !usr_dir.exists() {
        fs::create_dir_all(&usr_dir).unwrap();
    }
    if !usr_dir.join("lib").exists() && jail_root.join("lib").exists() {
        let _ = symlink("../lib", usr_dir.join("lib"));
    }
    if !usr_dir.join("bin").exists() && jail_root.join("bin").exists() {
        let _ = symlink("../bin", usr_dir.join("bin"));
    }
    if !usr_dir.join("share").exists() && jail_root.join("share").exists() {
        let _ = symlink("../share", usr_dir.join("share"));
    }
    if !jail_root.join("lib64").exists() && jail_root.join("lib").exists() {
        let _ = symlink("lib", jail_root.join("lib64"));
    }
    let machine_id_path = jail_root.join("etc/machine-id");
    if !machine_id_path.exists() {
        if Path::new("/etc/machine-id").exists() {
            let _ = fs::copy("/etc/machine-id", &machine_id_path);
        } else {
            let _ = fs::write(&machine_id_path, "5c02456317b34d6983792070381665ea\n");
        }
    }
    let var_lib_dbus = jail_root.join("var/lib/dbus");
    fs::create_dir_all(&var_lib_dbus).unwrap();
    if !var_lib_dbus.join("machine-id").exists() {
        let _ = symlink("/etc/machine-id", var_lib_dbus.join("machine-id"));
    }
    println!("🏗️  Building /run hierarchy...");
    let run_root = jail_root.join("run");
    let run_flatpak = run_root.join("flatpak");
    let run_host = run_root.join("host");
    let run_user = run_root.join("user").join(&uid);
    fs::create_dir_all(&run_root).unwrap();
    fs::create_dir_all(&run_flatpak).unwrap();
    fs::create_dir_all(&run_host).unwrap();
    fs::create_dir_all(&run_user).unwrap();
    let _ = fs::create_dir_all(run_flatpak.join("app"));
    let _ = fs::create_dir_all(run_flatpak.join("bus"));
    let _ = fs::create_dir_all(run_flatpak.join("ld.so.conf.d"));
    let _ = fs::create_dir_all(run_flatpak.join("p11-kit"));
    let info_content = format!(r#"
    [Instance]
    instance-id=flatvodka
    app-id={}
    arch=x86_64
    flatpak-version=1.14.0
    runtime-path=/usr
    original-app-path=/app
    "#, app_id);
    let _ = fs::write(run_user.join("flatpak-info"), info_content);
    let fbsd_fonts = Path::new("/usr/local/share/fonts");
    if fbsd_fonts.exists() {
        let target_fonts = run_host.join("fonts");
        fs::create_dir_all(&target_fonts).unwrap();
        println!("A  Mapping Host Fonts...");
        mount_bind(fbsd_fonts, &target_fonts, true);
        let xml_content = r#"<?xml version="1.0"?>
        <!DOCTYPE fontconfig SYSTEM "fonts.dtd">
        <fontconfig>
        <dir>/run/host/fonts</dir>
        <dir>/usr/local/share/fonts</dir>
        </fontconfig>
        "#;
        let _ = fs::write(run_host.join("font-dirs.xml"), xml_content);
    }
    let host_os_release = Path::new("/etc/os-release");
    if host_os_release.exists() {
        let _ = fs::copy(host_os_release, run_host.join("os-release"));
    }
    fs::create_dir_all(jail_root.join("tmp")).unwrap();
    let x11_host = Path::new("/tmp/.X11-unix");
    if x11_host.exists() {
        let x11_target = jail_root.join("tmp/.X11-unix");
        fs::create_dir_all(&x11_target).unwrap();
        println!("📺 Mounting X11 socket...");
        mount_bind(x11_host, &x11_target, false);
    }
    if let Ok(wl) = std::env::var("WAYLAND_DISPLAY") {
        let wl_host = PathBuf::from(format!("/var/run/user/{}/{}", uid, wl));
        if wl_host.exists() {
            let wl_target = run_user.join(&wl);
            let _ = fs::File::create(&wl_target);
            println!("🌊 Mounting Wayland Socket: {} -> {:?}", wl, wl_target);
            mount_bind(&wl_host, &wl_target, false);
        } else {
            println!("⚠️  Wayland requested but socket not found at {:?}", wl_host);
        }
    }
    let pulse_host = PathBuf::from(format!("/var/run/user/{}/pulse/native", uid));
    if pulse_host.exists() {
        let pulse_target_dir = run_user.join("pulse");
        fs::create_dir_all(&pulse_target_dir).unwrap();
        let pulse_target = pulse_target_dir.join("native");
        let _ = fs::File::create(&pulse_target);
        println!("🔊 Mounting PulseAudio...");
        mount_bind(&pulse_host, &pulse_target, false);
    }
    let atspi_host = PathBuf::from(format!("/var/run/xdg/{}/at-spi", user_name));
    if atspi_host.exists() {
        let atspi_jail_path = jail_root.join(format!("var/run/xdg/{}/at-spi", user_name));
        fs::create_dir_all(&atspi_jail_path).unwrap();
        println!("♿ Mounting Accessibility Bus...");
        mount_bind(&atspi_host, &atspi_jail_path, false);
    }
    let dbus_sys = Path::new("/var/run/dbus");
    if dbus_sys.exists() {
        let dbus_target = jail_root.join("var/run/dbus");
        fs::create_dir_all(&dbus_target).unwrap();
        println!("🚌 Mounting System DBus...");
        mount_bind(dbus_sys, &dbus_target, false);
    }
    fs::create_dir_all(jail_root.join("home/user")).unwrap();
    fs::create_dir_all(jail_root.join("dev")).unwrap();
    fs::create_dir_all(jail_root.join("proc")).unwrap();
    fs::create_dir_all(jail_root.join("sys")).unwrap();
    sys_mount("devfs", "devfs", jail_root.join("dev").to_str().unwrap(), false);
    sys_mount("linprocfs", "linprocfs", jail_root.join("proc").to_str().unwrap(), false);
    sys_mount("linsysfs", "linsysfs", jail_root.join("sys").to_str().unwrap(), false);
    let sh_check = jail_root.join("bin/sh");
    if !sh_check.exists() {
        eprintln!("❌ /bin/sh not found in jail");
        let _ = Command::new("umount").arg("-f").arg(&jail_root).output();
        std::process::exit(1);
    }
    println!("🔒 Creating jail: {}", jname);
    let jail_status = Command::new("jail")
    .arg("-c")
    .arg(format!("name={}", jname))
    .arg(format!("path={}", jail_root.display()))
    .arg("host.hostname=flatvodka")
    .arg("persist")
    .status()
    .expect("Failed to create jail");
    if !jail_status.success() {
        eprintln!("❌ Failed to create jail");
        let _ = Command::new("umount").arg("-f").arg(&jail_root).output();
        std::process::exit(1);
    }
    println!("✅ Jail created");
    let final_cmd = if !_raw_sockets {
        default_cmd
    } else {
        argv.get(0).map_or(default_cmd, |s| s.as_str())
    };
    let bin_path = if final_cmd.starts_with("/") {
        final_cmd.to_string()
    } else {
        format!("/app/bin/{}", final_cmd)
    };
    let host_bin_path = jail_root.join(bin_path.trim_start_matches('/'));
    if host_bin_path.exists() {
        println!("🏷️  Branding binary as LinuxELF: {:?}", host_bin_path);
        let _ = Command::new("brandelf")
        .arg("-t")
        .arg("Linux")
        .arg(&host_bin_path)
        .output();
    }
    println!("🎬 Executing: {}", bin_path);
    let typelib_path = format!(
        "/app/lib/girepository-1.0:/usr/lib/girepository-1.0:/usr/lib/x86_64-linux-gnu/girepository-1.0:/lib/girepository-1.0"
    );
    let mut loaders_cache = String::from("/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache");
    let cache_candidates = vec![
        "usr/lib/x86_64-linux-gnu/gdk-pixbuf-2.0/2.10.0/loaders.cache",
        "usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache",
        "lib/x86_64-linux-gnu/gdk-pixbuf-2.0/2.10.0/loaders.cache",
        "lib/gdk-pixbuf-2.0/2.10.0/loaders.cache",
    ];
    for cand in cache_candidates {
        if jail_root.join(cand).exists() {
            loaders_cache = format!("/{}", cand);
            println!("🖼️  Found Pixbuf Loaders: {}", loaders_cache);
            break;
        }
    }
    let lib_path = format!(
        "/app/lib:/app/lib64:/lib/x86_64-linux-gnu:/usr/lib/x86_64-linux-gnu:/lib64:/lib:/usr/lib64:/usr/lib"
    );
    let mut vk_icds = Vec::new();
    let icd_dirs = vec!["/compat/linux/usr/share/vulkan/icd.d", "/usr/share/vulkan/icd.d"];
    for dir in icd_dirs {
        if Path::new(dir).exists() {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    if entry.path().extension().map(|s| s == "json").unwrap_or(false) {
                        vk_icds.push(entry.path().to_string_lossy().to_string());
                    }
                }
            }
        }
    }
    let mut vk_layers = Vec::new();
    let layer_dirs = vec!["/compat/linux/usr/share/vulkan/explicit_layer.d", "/usr/share/vulkan/explicit_layer.d"];
    for dir in layer_dirs {
        if Path::new(dir).exists() {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    if entry.path().extension().map(|s| s == "json").unwrap_or(false) {
                        vk_layers.push(entry.path().to_string_lossy().to_string());
                    }
                }
            }
        }
    }
    println!("🛠️  Vulkan ICDs: {:?}", vk_icds);
    println!("🛠️  Vulkan Layers: {:?}", vk_layers);
    let mut gl_lib_dirs = Vec::new();
    let gl_search_dirs = vec![
        "/compat/linux/usr/lib",
        "/compat/linux/usr/lib64",
        "/usr/lib",
        "/usr/lib64",
        "/lib",
        "/lib64",
        "/compat/linux/usr/lib/dri",
        "/compat/linux/usr/lib64/dri",
    ];
    for dir in gl_search_dirs {
        if Path::new(dir).exists() {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if let Some(fname) = path.file_name().and_then(|s| s.to_str()) {
                        if fname.starts_with("libGL") || fname.starts_with("libEGL") || fname.starts_with("libGLX") {
                            gl_lib_dirs.push(path.to_string_lossy().to_string());
                        }
                    }
                }
            }
        }
    }
    println!("🛠️  OpenGL libraries found: {:?}", gl_lib_dirs);
    fn inject_missing_libs(libs: &[&str], jail_root: &Path) {
        let compat_dirs = vec![
            "/compat/ubuntu/lib",
            "/compat/ubuntu/lib64",
            "/compat/linux/usr/lib",
            "/compat/linux/usr/lib64",
        ];
        for lib_name in libs {
            let mut found_in_jail = false;
            for subdir in &["lib", "lib64"] {
                let target_path = jail_root.join(subdir).join(lib_name);
                if target_path.exists() {
                    found_in_jail = true;
                    break;
                }
            }
            if found_in_jail {
                continue;
            }
            let mut copied = false;
            for dir in &compat_dirs {
                let host_path = Path::new(dir).join(lib_name);
                if host_path.exists() {
                    let target_dir = if dir.ends_with("64") { "lib64" } else { "lib" };
                    let target_path = jail_root.join(target_dir);
                    if !target_path.exists() { let _ = fs::create_dir_all(&target_path); }
                    let target_file = target_path.join(lib_name);
                    if let Err(e) = fs::copy(&host_path, &target_file) {
                        eprintln!("❌ Failed to copy {} -> {}: {}", host_path.display(), target_file.display(), e);
                    } else {
                        println!("✅ Injected {} into jail: {}", lib_name, target_file.display());
                        copied = true;
                        break;
                    }
                }
            }
            if !copied {
                eprintln!("⚠️  Could not find {} in any compat directory!", lib_name);
            }
        }
    }
    inject_missing_libs(
        &["libGLEW.so.2.2", "libGL.so.1", "libGLU.so.1", "libEGL.so.1", "libGLESv2.so.2"],
        &jail_root,
    );
    let lib_path = format!(
        "/app/lib:/app/lib64:/lib/x86_64-linux-gnu:/usr/lib/x86_64-linux-gnu:/lib64:/lib:/usr/lib64:/usr/lib"
    );
    let shell_cmd = format!(
        "export LD_LIBRARY_PATH=\"{}\"; \
export TERM=xterm-256color; \
export container=flatpak; \
export FLATPAK_ID=\"{}\"; \
export HOME=/home/user; \
export USER=user; \
export XDG_RUNTIME_DIR=/run/user/{}; \
export PATH=/app/bin:/usr/bin:/bin:/sbin:/usr/sbin; \
export XDG_DATA_DIRS=/app/share:/usr/share:/share; \
export XDG_CONFIG_DIRS=/app/etc/xdg:/etc/xdg; \
export XDG_CACHE_HOME=/home/user/.cache; \
export GI_TYPELIB_PATH=\"{}\"; \
export GDK_PIXBUF_MODULE_FILE=\"{}\"; \
export GST_PLUGIN_SYSTEM_PATH=/app/lib/gstreamer-1.0:/usr/lib/extensions/gstreamer-1.0:/usr/lib/x86_64-linux-gnu/gstreamer-1.0; \
export XDG_CURRENT_DESKTOP=GNOME; \
export LANG=C.UTF-8; \
exec \"{}\"",
lib_path, app_id, uid, typelib_path, loaders_cache, bin_path
    );
    let mut cmd = Command::new("/usr/sbin/chroot");
    cmd.arg(&jail_root);
    cmd.arg("/bin/sh");
    cmd.arg("-c");
    cmd.arg(shell_cmd);
    if let Ok(display) = std::env::var("DISPLAY") {
        cmd.env("DISPLAY", display);
    }
    if let Ok(wl) = std::env::var("WAYLAND_DISPLAY") {
        cmd.env("WAYLAND_DISPLAY", wl);
    }
    match cmd.spawn() {
        Ok(mut child) => {
            let status = child.wait();
            let _ = Command::new("jail").arg("-r").arg(&jname).output();
            println!("🛑 App finished.");
            println!("💾 Filesystem is STILL MOUNTED for debugging at: {}", jail_root.display());
            if let Ok(s) = status {
                if let Some(code) = s.code() {
                    std::process::exit(code);
                }
            }
        }
        Err(e) => {
            eprintln!("❌ Failed to spawn: {}", e);
            let _ = Command::new("jail").arg("-r").arg(&jname).output();
            std::process::exit(1);
        }
    }
    std::process::exit(1);
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Install { target } => install_logic(&target),
        Commands::Run {
            app_id,
            argv,
            raw_sockets,
        } => run_app(&app_id, argv, raw_sockets),
        Commands::List => {
            for e in WalkDir::new(get_flatpak_dir().join("app"))
                .min_depth(1)
                .max_depth(1)
                {
                    if let Ok(entry) = e {
                        println!("{}", entry.file_name().to_string_lossy());
                    }
                }
        }
        Commands::Clean => {}
    }
}
