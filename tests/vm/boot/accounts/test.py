# Accounts across a keep on a real machine: users and a service user created, and users deleted (one of them declared), after a snapshot, then `archix rollback N` and a reboot. The ones created since come along with their passwords, and every one deleted since comes back as a locked placeholder (password "!", expired, nologin), so its UID stays reserved; `archix user unlock` re-enables the declared one and refuses the undeclared one.
from boot.common import HOST, bind, boot, edit_config, out, snapshots, switch


def shadow(name):
    return out(f"getent shadow {name}").strip().split(":")


def passwd(name):
    return out(f"getent passwd {name}").strip().split(":")


def run(m, t):
    bind(m, t)
    with t.step("1: a declared user and an undeclared one, then a snapshot"):
        boot()
        edit_config("'s|^return {|return { users = { erin = {} },|'")
        switch("declaring erin")
        m.succeed("useradd -m -s /bin/bash dave && echo dave:pw0 | chpasswd", what="an undeclared user before the snapshot")
        said = m.succeed("archix snapshot create 2>&1", what="a manual snapshot")
        number = snapshots()[-1]
        t.log(f"snapshot {number}: {said}")
        uids = {name: passwd(name)[2] for name in ("erin", "dave")}

    with t.step("2: after it, users made and deleted; the keep names the placeholders"):
        m.succeed("useradd -m -s /bin/bash carol && echo carol:pw2 | chpasswd && useradd -r -s /usr/bin/nologin svc && userdel dave && userdel erin", what="accounts changed after the snapshot")
        said = m.succeed(f"{HOST} rollback {number} --noconfirm 2>&1", what=f"keeping snapshot {number}")
        t.log(said)
        t.check("dave was deleted since" in said and "archix user unlock erin" in said, f"the keep should name the placeholders and how to unlock the declared one: {said[-800:]}")
        m.reboot()

    with t.step("3: after the reboot, the accounts are there and the deleted ones locked placeholders"):
        boot()
        t.check(passwd("carol")[-1] == "/bin/bash" and shadow("carol")[1].startswith("$"), f"carol, made since, comes with her password: {passwd('carol')} {shadow('carol')[:2]}")
        t.check(len(passwd("svc")) > 1, "the service user made since comes too")
        for name in ("dave", "erin"):
            p, s = passwd(name), shadow(name)
            t.check(p[-1] == "/usr/bin/nologin" and s[1] == "!" and s[7] == "1" and p[2] == uids[name], f"{name}, deleted since, is a locked placeholder with its UID {uids[name]}: {p} {s}")
        code, said = m.execute(f"{HOST} user unlock dave 2>&1")
        t.check(code != 0 and "isn't declared" in said, f"an undeclared placeholder stays locked: {said}")
        m.succeed(f"{HOST} user unlock erin 2>&1", what="unlocking the declared one")
        t.check(passwd("erin")[-1] != "/usr/bin/nologin" and shadow("erin")[7] == "", f"unlock re-enables erin: {passwd('erin')} {shadow('erin')}")
