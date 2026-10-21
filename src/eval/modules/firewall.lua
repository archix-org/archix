local t = archix.types

local range = t.submodule {
  from = archix.option { type = t.int },
  to = archix.option { type = t.int },
}

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
    if cfg.firewall.enable then
      return { packages = { "nftables" } }
    end
    return {}
  end,
}
