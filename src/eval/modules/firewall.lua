local t = archix.types

local range = t.submodule {
  from = archix.option { type = t.int },
  to = archix.option { type = t.int },
}

local function span(from, to) return { from = from, to = to } end

local services = {
  -- nixpkgs programs.alvr.openFirewall
  alvr = { tcp = { 9943, 9944 }, udp = { 9943, 9944 } },
  -- nixpkgs programs.kdeconnect.openFirewall, whose package may be gnomeExtensions.gsconnect
  gsconnect = { tcp_ranges = { span(1714, 1764) }, udp_ranges = { span(1714, 1764) } },
  -- home-assistant.io/integrations/http server port default for Container and Core; nixpkgs binds *:8123
  ["home-assistant"] = { tcp = { 8123 } },
  -- RFC 9110 section 4.2.1
  http = { tcp = { 80 } },
  -- RFC 9110 section 4.2.2
  https = { tcp = { 443 } },
  -- nixpkgs services.jellyfin.openFirewall, from jellyfin.org's networking docs
  jellyfin = { tcp = { 8096, 8920 }, udp = { 1900, 7359 } },
  -- nixpkgs programs.kdeconnect.openFirewall
  kdeconnect = { tcp_ranges = { span(1714, 1764) }, udp_ranges = { span(1714, 1764) } },
  -- nixpkgs programs.localsend.openFirewall
  localsend = { tcp = { 53317 }, udp = { 53317 } },
  -- RFC 6762; nixpkgs services.avahi.openFirewall
  mdns = { udp = { 5353 } },
  -- nixpkgs services.minecraft-server.openFirewall at the default server-port
  minecraft = { tcp = { 25565 }, udp = { 25565 } },
  -- nixpkgs programs.mosh.openFirewall
  mosh = { udp_ranges = { span(60000, 61000) } },
  -- NFSv4, which needs no rpcbind: nfs(5) on port=
  nfs = { tcp = { 2049 } },
  -- nixpkgs services.plex.openFirewall
  plex = { tcp = { 3005, 8324, 32400, 32469 }, udp = { 1900, 5353, 32410, 32412, 32413, 32414 } },
  -- nixpkgs services.samba.openFirewall
  samba = { tcp = { 139, 445 }, udp = { 137, 138 } },
  -- RFC 4253 section 4.1; nixpkgs services.openssh.ports
  ssh = { tcp = { 22 } },
  -- nixpkgs programs.steam remotePlay and localNetworkGameTransfers openFirewall
  steam = { tcp = { 27036, 27037, 27040 }, udp = { 10400, 10401, 27036 }, udp_ranges = { span(27031, 27035) } },
  -- nixpkgs programs.steam.dedicatedServer.openFirewall
  ["steam-dedicated-server"] = { tcp = { 27015 }, udp = { 27015 } },
  -- nixpkgs services.sunshine.openFirewall at the default base port 47989
  sunshine = { tcp = { 47984, 47989, 47990, 48010 }, udp = { 47998, 47999, 48000, 48002, 48010 } },
  -- nixpkgs services.syncthing.openDefaultPorts
  syncthing = { tcp = { 22000 }, udp = { 21027, 22000 } },
  -- nixpkgs services.tailscale.port default
  tailscale = { udp = { 41641 } },
  -- nixpkgs services.transmission.openPeerPorts at the default peer-port; the RPC port stays closed
  transmission = { tcp = { 51413 }, udp = { 51413 } },
  -- wireguard.com/quickstart's listen-port; WireGuard itself has no fixed port
  wireguard = { udp = { 51820 } },
  -- nixpkgs services.wivrn.openFirewall
  wivrn = { tcp = { 9757 }, udp = { 9757 } },
}

local builtin_names = {}
for name in pairs(services) do builtin_names[#builtin_names + 1] = name end

return {
  options = {
    firewall = {
      enable = archix.option {
        type = t.bool,
        default = false,
        description = "An nftables firewall in its own `inet archix` table, loaded by archix-firewall.service; like every change to it, it applies at the next boot. Incoming traffic is dropped unless it answers outgoing traffic or something below allows it; other tables (Docker's, Tailscale's) are left alone.",
      },
      allowed_tcp_ports = archix.option { type = t.list_of(t.int), default = {}, description = "TCP ports open to everyone." },
      allowed_udp_ports = archix.option { type = t.list_of(t.int), default = {}, description = "UDP ports open to everyone." },
      allowed_tcp_port_ranges = archix.option { type = t.list_of(range), default = {}, description = "TCP port ranges open to everyone, as { from, to }." },
      allowed_udp_port_ranges = archix.option { type = t.list_of(range), default = {}, description = "UDP port ranges open to everyone, as { from, to }." },
      allowed_services = archix.option {
        type = t.list_of(t.str),
        default = {},
        description = "Services whose ports, from firewall.service_ports, are open to everyone, such as { \"ssh\", \"syncthing\" }; each service's rules carry its name as their nftables comment. Naming a service service_ports doesn't define is an error.",
      },
      service_ports = archix.option {
        type = t.attrs_of(t.submodule {
          tcp = archix.option { type = t.list_of(t.int), default = {}, description = "TCP ports the service listens on." },
          udp = archix.option { type = t.list_of(t.int), default = {}, description = "UDP ports the service listens on." },
          tcp_ranges = archix.option { type = t.list_of(range), default = {}, description = "TCP port ranges, as { from, to }." },
          udp_ranges = archix.option { type = t.list_of(range), default = {}, description = "UDP port ranges, as { from, to }." },
        }),
        default = {},
        description = "The ports of each service firewall.allowed_services can name. archix defines " .. table.concat(builtin_names, ", ") .. " with archix.default, so a plain definition of one of these names replaces it; other names add services.",
      },
      trusted_interfaces = archix.option { type = t.list_of(t.str), default = {}, description = "Interfaces whose incoming traffic is always accepted, such as a VPN's." },
      allow_ping = archix.option { type = t.bool, default = true, description = "Answer ICMP echo requests. ICMPv6, which IPv6 needs to work, is always accepted." },
      check_reverse_path = archix.option {
        type = t.enum { "strict", "loose", "off" },
        default = "strict",
        description = "Drop packets whose source address wouldn't be routed back out the interface they came in on (strict), out of any interface (loose), or don't check (off). VPNs that route asymmetrically may need loose.",
      },
      extra_input_rules = archix.option { type = t.lines, default = "", description = "nftables rules added to the end of the input chain, such as `ip saddr 192.168.0.0/24 tcp dport 8080 accept`." },
      extra_rules = archix.option { type = t.lines, default = "", description = "More nftables chains or sets inside the archix table." },
    },
  },
  config = function(cfg)
    local defined = cfg.firewall.service_ports
    local unknown = {}
    for _, name in ipairs(cfg.firewall.allowed_services) do
      if defined[name] == nil and services[name] == nil then unknown[#unknown + 1] = string.format("%q", name) end
    end
    if #unknown > 0 then
      local known = {}
      for name in pairs(defined) do known[name] = true end
      for name in pairs(services) do known[name] = true end
      local names = {}
      for name in pairs(known) do names[#names + 1] = name end
      error("firewall.allowed_services names " .. table.concat(unknown, ", ") .. ", which firewall.service_ports doesn't define; known services: " .. table.concat(names, ", "), 0)
    end
    local ports = {}
    for name, spec in pairs(services) do ports[name] = archix.default(spec) end
    local out = { firewall = { service_ports = ports } }
    if cfg.firewall.enable then out.packages = { "nftables" } end
    return out
  end,
}
