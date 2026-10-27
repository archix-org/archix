use std::fs;

use anyhow::{Context, Result, bail};

use archix::deploy::{self, layout};
use archix::paths::Paths;
use archix::system::{self, activate};

use super::require_root;

/// `archix hooks retry|skip`: runs the hooks waiting for a deployment now, or drops them, after they failed at too many boots.
pub fn hooks(retry: bool, number: Option<u32>) -> Result<()> {
    require_root("hooks")?;
    let paths = Paths::system();
    let booted = system::boot::booted_deployment(&paths.root);
    let number = match number.or(booted) {
        Some(number) => number,
        None => bail!("no deployment is running; name one"),
    };
    let record = system::hooks::pending_path(&paths.state, number);
    if !record.exists() {
        println!("no hooks wait for deployment {number}");
        return Ok(());
    }
    let key = activate::hooks_key(number);
    if !retry {
        fs::remove_file(&record).with_context(|| format!("removing {}", record.display()))?;
        activate::clear_tries(&paths.state, &key);
        println!("dropped the hooks waiting for deployment {number}; they won't run");
        return Ok(());
    }
    if booted != Some(number) {
        bail!("deployment {number} isn't the one running, so its hooks run at its boot");
    }
    let failures = system::hooks::run_pending(&paths.root, &paths.state, number);
    if failures.is_empty() {
        activate::clear_tries(&paths.state, &key);
        println!("deployment {number}'s hooks ran");
        return Ok(());
    }
    bail!(
        "{}; they stay waiting, `archix hooks skip` drops them",
        failures.join("; ")
    )
}

/// `archix verify`: checks each kept deployment's usr, opt and boot against its packages, and with `repair` restores what differs from the package cache.
pub fn verify(repair: bool) -> Result<()> {
    require_root("verify")?;
    verify_in(&Paths::system(), repair, &[], "sudo archix verify --repair")
}

/// `archix verify` on the system whose root and state `paths` name, for the deployments `only` lists or every kept one; `repair_hint` is the command that repairs.
pub fn verify_in(paths: &Paths, repair: bool, only: &[u32], repair_hint: &str) -> Result<()> {
    let paths = paths.clone();
    if !layout::laid_out(&paths.root) {
        println!("there are no deployments yet");
        return Ok(());
    }
    let store = deploy::Store::open(&paths.root)?;
    let mut changed = Vec::new();
    let kept = store.deployments()?;
    if let Some(missing) = only.iter().find(|n| !kept.contains(n)) {
        bail!("there's no complete deployment {missing}");
    }
    for number in kept.into_iter().filter(|n| only.is_empty() || only.contains(n)) {
        let deployment = store.path(number);
        let (mut problems, mut missing) = deploy::verify::changed(deploy::verify::check(&deployment)?);
        if !problems.is_empty() && repair {
            let _lock = store.lock()?;
            let repaired = deploy::verify::repair(&paths, &deployment, &problems)
                .with_context(|| format!("repairing deployment {number}"))?;
            println!("deployment {number}: repaired {repaired} files from the package cache");
            (problems, missing) = deploy::verify::changed(deploy::verify::check(&deployment)?);
        }
        deploy::verify::save(&paths, number, &problems);
        if missing > 0 {
            println!(
                "deployment {number}: {missing} packaged files are missing, as pacman.conf's NoExtract or a deletion left them"
            );
        }
        if problems.is_empty() {
            println!("deployment {number}: matches its packages");
        } else {
            println!(
                "deployment {number}: {} files differ from its packages:\n{}",
                problems.len(),
                deploy::verify::describe(&problems, 20)
            );
            changed.push(number);
        }
    }
    if !changed.is_empty() {
        let hint = if repair {
            String::new()
        } else {
            format!("; `{repair_hint}` restores them")
        };
        bail!(
            "{} changed{hint}",
            changed
                .iter()
                .map(|n| format!("deployment {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

/// `archix mount-deployment`, from a live USB: binds a deployment's trees over the mounted archix root and says what to run next.
pub fn mount_deployment(root: &std::path::Path, number: Option<u32>) -> Result<()> {
    require_root("mount-deployment")?;
    let number = system::deploy_boot::mount_for_chroot(root, number)?;
    println!("bound deployment {number}'s trees over {}", root.display());
    let esp = fs::read_to_string(Paths::under(root).state.join("boot/esp")).unwrap_or_default();
    let esp = esp.trim().trim_start_matches('/');
    if !esp.is_empty() && !system::boot::is_fat(&root.join(esp)) {
        println!(
            "mount the ESP at {} next, as boot entries go there",
            root.join(esp).display()
        );
    }
    println!(
        "then `arch-chroot {root}`, and there `archix rollback {number}` makes deployment {number} current and writes its boot entry; `umount -R {root}` once you're done",
        root = root.display()
    );
    Ok(())
}
