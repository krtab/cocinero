There are two things that people keep reimplementing: declarative configuration tools, and static site generators. This one is not a static site generator.

```toml
# Top-level list of packages to install via apt-get.
# Optional. Can be empty or omitted.
packages = ["curl", "git", "htop"]

# List of systemd units to enable and reload after receipe.
# Optional. Can be empty or omitted.
systemd = ["nginx.service", "docker.service"]

# Template variables for steps marked with template = true
# Optional. Each table is a separate set of variables used for rendering templates.
template_vars = [
  { username = "alice", home = "/home/alice" },
  { username = "bob",   home = "/home/bob" }
]

# Steps define the actions executed in order.
# Each step must have a 'kind' field: install/copy, shell, or run
[[steps]]
kind = "install"            # Can also be "copy" as alias
template = true             
src = "configs/myconfig.conf"    # Relative source file path (not template-able)
dest = "/etc/{{username}}_myconfig.conf"  # Templated destination path (template-able)
mode = "0644"               # Optional file permissions (not template-able)

[[steps]]
kind = "install"
template = false            # Simple file copy, no templating
src = "scripts/setup_helper.sh"
dest = "/usr/local/bin/setup_helper.sh"
# mode omitted, will be copied from source

[[steps]]
kind = "shell"
cmd = "echo 1 >> /etc/is_multi_users"    # Simple shell command
# template = false by default

[[steps]]
kind = "shell"
template = true             # Template shell command
cmd = "usermod -a -G audio {{username}}"
# Will be rendered once per entry in template_vars

[[steps]]
kind = "run"
script = "scripts/init.sh"  # Relative script path
# template = false by default

[[steps]]
kind = "run"
template = true
script = "scripts/setup.sh"  # Template script
# Will be rendered and run once per table in template_vars
```