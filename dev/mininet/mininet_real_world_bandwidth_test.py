#!/usr/bin/env python3
from mininet.net import Mininet
from mininet.node import OVSController
from mininet.link import TCLink
from mininet.log import setLogLevel, info
from mininet.cli import CLI
import argparse
import os
import time
import threading

def link_bandwidth_modifier(net, delay_seconds=300, target_bw=10):
    time.sleep(delay_seconds)

    relay = net.get('relay')
    sub = net.get('sub')
    s1 = net.get('s1')

    timestamp = time.strftime("%H:%M:%S")

    try:
        link_relay = relay.connectionsTo(s1)[0]
        link_sub = sub.connectionsTo(s1)[0]

        msg = f"\n[{timestamp}] *** Link sávszélesség módosítás: relay és sub linkek -> {target_bw} Mbps\n"
        print(msg, flush=True)
        info(msg)

        link_relay[0].config(bw=target_bw)
        link_sub[0].config(bw=target_bw)

        success_msg = f"[{timestamp}] *** Módosítás sikeres!\n"
        print(success_msg, flush=True)
        info(success_msg)

    except Exception as e:
        error_msg = f"[{timestamp}] *** Link módosítás hiba: {e}\n"
        print(error_msg, flush=True)
        info(error_msg)

def start(bw_pr=500.0, bw_bottleneck=500.0, bw_sr=500.0, delay_ms='10ms', loss=0.0):
    setLogLevel('info')
    net = Mininet(controller=OVSController, link=TCLink, autoSetMacs=True)

    s1 = net.addSwitch('s1')
    c0 = net.addController('c0')

    pub   = net.addHost('pub',   ip='10.0.0.1/24')
    relay = net.addHost('relay', ip='10.0.0.2/24')
    sub   = net.addHost('sub',   ip='10.0.0.3/24')

    # Linkek
    net.addLink(pub,   s1, bw=bw_pr,         delay=delay_ms, loss=loss, max_queue_size=600)
    net.addLink(relay, s1, bw=bw_bottleneck, delay=delay_ms, loss=loss, max_queue_size=600)
    net.addLink(sub,   s1, bw=bw_sr,         delay=delay_ms, loss=loss, max_queue_size=600)

    net.start()

    # Repo gyökér mappa
    root = os.path.abspath(os.path.join(os.path.dirname(__file__), '..'))
    if os.path.basename(root) == "dev":
        root = os.path.dirname(root)

    # Tanúsítvány ha nincs meg
    if not os.path.exists(os.path.join(root, 'dev', 'localhost.crt')):
        os.system(f'cd {root} && ./dev/cert')

    # Relay indítása
    relay_cmd = f'{root}/dev/relay_for_mininet > relay.log 2>&1 &'
    info(f'*** start relay: {relay_cmd}\n')
    relay.cmd(relay_cmd)
    time.sleep(5)

    # Publisher indítása
    pub_input = os.path.join(root, 'dev', 'bbb.fmp4')
    if not os.path.exists(pub_input):
        info('*** dev/bbb.fmp4 hiányzik, futtasd előtte ./dev/pub_for_mininet a hoston a letöltéshez/konvertáláshoz\n')
    pub_cmd = f'{root}/dev/pub_for_mininet > pub.log 2>&1 &'
    info(f'*** start publisher: {pub_cmd}\n')
    pub.cmd(pub_cmd)
    time.sleep(3)

    # Subscriber indítása
    sub_cmd = f'{root}/dev/sub_for_mininet > sub.log 2>&1 &'
    info(f'*** start subscriber: {sub_cmd}\n')
    sub.cmd(sub_cmd)

    info('*** fut: relay.log / pub.log / sub.log a repo gyökerében\n')
    info('*** Mininet CLI: pl. link újrakonfigurálás: link s1-relay bw 0.3 delay 80ms\n')

    link_thread = threading.Thread(
        target=link_bandwidth_modifier,
        args=(net, 298, 10),
        daemon=True
    )
    link_thread.start()
    info(f'*** Link módosítás ütemezve 5 perc múlva (300 másodperc)\n')
    info(f'*** Célsávszélesség: 10 Mbps (relay és sub linkek)\n')

    CLI(net)
    net.stop()

if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--bw-pr', type=float, default=100.0, help='pub–switch bw (Mbit/s)')
    parser.add_argument('--bw-bottleneck', type=float, default=100.0, help='relay–switch bw (Mbit/s)')
    parser.add_argument('--bw-sr', type=float, default=100.0, help='sub–switch bw (Mbit/s)')
    parser.add_argument('--delay', default='3ms', help='link delay (e.g. 50ms)')
    parser.add_argument('--loss', type=float, default=0.0, help='packet loss percent')
    args = parser.parse_args()
    start(args.bw_pr, args.bw_bottleneck, args.bw_sr, args.delay, args.loss)
