local t = archix.types

return {
  options = {
    secrets = {
      machine_key = archix.option {
        type = t.nullable(t.str),
        default = archix.null,
        description = "This machine's age recipient (age1...), which `sudo archix secrets init` makes and prints; every secret this host declares is encrypted to it.",
      },
      admins = archix.option {
        type = t.list_of(t.str),
        default = {},
        description = "Your own age, age plugin (such as age-plugin-yubikey) or ssh-ed25519 recipients, which every secret is encrypted to as well: the way back to them if a machine's key is lost. Needed once any secret is declared, unless without_admin = true.",
      },
      without_admin = archix.option {
        type = t.bool,
        default = false,
        description = "Declare secrets with no admin recipient, knowing that losing this machine's key then loses every secret.",
      },
      allow_hibernation = archix.option {
        type = t.bool,
        default = false,
        description = "Let the machine hibernate while secrets are declared and a swap isn't encrypted; hibernating writes all of memory, every decrypted secret with it, to that swap.",
      },
      items = archix.option {
        type = t.attrs_of(t.submodule {
          file = archix.option { type = t.str, description = "The age file in the config directory, such as secrets/wg0.age; `archix secrets edit` writes it." },
          path = archix.option {
            type = t.nullable(t.str),
            default = archix.null,
            description = "Also write the secret to this file under /run at each boot, for apps that refuse a link, such as a NetworkManager profile in /run/NetworkManager/system-connections.",
          },
          owner = archix.option { type = t.str, default = "root" },
          group = archix.option { type = t.str, default = "root" },
          mode = archix.option { type = t.str, default = "0400" },
          kind = archix.option {
            type = t.enum { "file", "crypt_hash" },
            default = "file",
            description = "file, or crypt_hash for a password hash that users.<name>.hashed_password = archix.secret(name) sets at each boot.",
          },
        }),
        default = {},
        description = "Secrets by name, decrypted at each boot of the deployment into /run/archix/secrets/<name> (a ramfs, never on disk); files[path] = { secret = name } links to one, archix.secret(name) puts one into a file's text, and units read archix.secret_path(name) through EnvironmentFile= or LoadCredential=.",
      },
    },
  },
}
