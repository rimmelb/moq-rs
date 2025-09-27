#!/usr/bin/env python3
from mininet.net import Mininet
from mininet.node import OVSController
from mininet.link import TCLink
from mininet.log import setLogLevel, info
from mininet.cli import CLI
import argparse
import math
import os

def bdp_queue_pkts(bw_mbps, rtt_ms, mss_bytes=1200, factor=1.5):
    """Durva becslés: queue ~ factor * BDP (pkt)"""
    bps = bw_mbps * 1e6
    bdp_bytes = bps * (rtt_ms/1000.0) / 8.0
    pkts = max(100, int(factor * bdp_bytes / mss_bytes))
    return pkts

def main():
    parser = argparse.ArgumentParser(description="Mininet topology for moq.rs (pub–relay–sub)")
    parser.add_argument("--bw-pub-relay", type=float, default=5.0, help="bw pub↔relay (Mbps)")
    parser.add_argument("--bw-relay-sub", type=float, default=5.0, help="bw relay↔sub (Mbps)")
    parser.add_argument("--delay-pub-relay", default="10ms", help="RTT/2-ish delay string for pub↔relay (e.g. 10ms)")
    parser.add_argument("--delay-relay-sub", default="10ms", help="RTT/2-ish delay string for relay↔sub")
    parser.add_argument("--loss-pub-relay", type=float, default=0.0, help="loss % pub↔relay")
    parser.add_argument("--loss-relay-sub", type=float, default=0.0, help="loss % relay↔sub")
    parser.add_argument("--rtt-hint-ms", type=int, default=10, help="RTT hint you pass to your QUIC (for window calc)")

    # opcionális: automatikus indítás a te skriptjeiddel
    parser.add_argument("--auto-run", action="store_true", help="start relay/publisher/subscriber commands")
    parser.add_argument("--relay-cmd", default="./relay_with_bandwidth",
                        help="relay indító bináris/script (host=relay)")
    parser.add_argument("--pub-cmd", default="./pub_with_bandwidth",
                        help="publisher bináris/script (host=pub)")
    parser.add_argument("--sub-cmd", default="./sub",
                        help="subscriber bináris/script (host=sub)")
    parser.add_argument("--relay-port", type=int, default=4443, help="relay QUIC/HTTPS port")

    args = parser.parse_args()

    setLogLevel("info")
    info("*** Building network\n")

    # Queue méret — a bw és a rtt alapján
    q_pub_relay = bdp_queue_pkts(args.bw_pub_relays if hasattr(args, 'bw_pub_relays') else args.bw_pub_relay,
                                 args.rtt_hint_ms)
    q_relay_sub = bdp_queue_pkts(args.bw_relay_sub, args.rtt_hint_ms)

    net = Mininet(controller=OVSController, link=TCLink, autoSetMacs=True, autoStaticArp=True)

    c0 = net.addController('c0')

    # Három host
    pub = net.addHost('pub', ip='10.0.0.1/24')
    relay = net.addHost('relay', ip='10.0.0.2/24')
    sub = net.addHost('sub', ip='10.0.0.3/24')

    # Két switch, hogy a két szakasz külön formázható legyen
    s1 = net.addSwitch('s1')
    s2 = net.addSwitch('s2')

    # Linkek
    # pub -- s1
    net.addLink(pub, s1,
                bw=args.bw_pub_relay,
                delay=args.delay_pub_relay,
                loss=args.loss_pub_relay,
                max_queue_size=q_pub_relay)

    # s1 -- relay
    net.addLink(s1, relay,
                bw=args.bw_pub_relay,
                delay=args.delay_pub_relay,
                loss=args.loss_pub_relay,
                max_queue_size=q_pub_relay)

    # relay -- s2
    net.addLink(relay, s2,
                bw=args.bw_relays if hasattr(args, 'bw_relays') else args.bw_relay_sub,
                delay=args.delay_relay_sub,
                loss=args.loss_relay_sub,
                max_queue_size=q_relay_sub)

    # s2 -- sub
    net.addLink(s2, sub,
                bw=args.bw_relays if hasattr(args, 'bw_relays') else args.bw_relay_sub,
                delay=args.delay_relay_sub,
                loss=args.loss_relay_sub,
                max_queue_size=q_relay_sub)

    info("*** Starting network\n")
    net.start()

    # Routing: egy broadcast domain, default gw nem kell; ARP autoStaticArp=true

    # Opcionális auto-run: indítsuk a te binárisaidat a megfelelő hostokon
    if args.auto_run:
        info("*** Launching relay on 10.0.0.2:{}\n".format(args.relay_port))
        # Figyelem a certre: IP-SAN vagy --tls.insecure jellegű flag kellhet
        relay_cmd = (
            f"{args.relay_cmd} "
            f"--bind [::]:{args.relay_port} "
            f"--rtt-ms {args.rtt_hint_ms} "
        )
        relay.popen(relay_cmd, shell=True)

        info("*** Launching subscriber on sub (connect to relay)\n")
        # ha HTTPS-t használsz és self-signed cert, kellhet egy --tls.insecure jellegű opció
        # moqt sémát is használhatsz, ha támogatott: moqt://10.0.0.2:{port}
        sub_url = f"https://10.0.0.2:{args.relay_port}"
        sub_cmd = f"{args.sub_cmd} {sub_url}"
        sub.popen(sub_cmd, shell=True)

        info("*** Launching publisher on pub (connect to relay)\n")
        pub_url = f"https://10.0.0.2:{args.relay_port}"
        # állítsd a saját argumentumaidat (name/fps/bitrate/tls/rtt/rate_limit), pl.:
        pub_cmd = (
            f"{args.pub_cmd} "
            f"--name desk/1.m4s "
            f"--fps 24 --bitrate 1500000 "
            f"--initial-rtt-ms {args.rtt_hint_ms} "
            f"{pub_url}"
        )
        pub.popen(pub_cmd, shell=True)

        info("*** Processes started. Use the CLI to monitor.\n")

    info("*** Ready. Type 'exit' to stop.\n")
    CLI(net)

    info("*** Stopping network\n")
    net.stop()

if __name__ == "__main__":
    main()
