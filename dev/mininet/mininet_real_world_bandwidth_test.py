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

def bandwidth_controller(net, bandwidth_file):
    """Háttérszál a sávszélesség vezérléshez"""
    relay = net.get('relay')

    # Kis várakozás, hogy a relay teljesen elinduljon
    time.sleep(8)

    log_file = 'bandwidth_controller.log'

    try:
        with open(log_file, 'w') as log:
            log.write(f"Bandwidth Controller - Started at {time.strftime('%Y-%m-%d %H:%M:%S')}\n")
            log.write("="*60 + "\n")
            log.flush()

            with open(bandwidth_file, 'r') as f:
                for line in f:
                    mbps = line.strip()
                    if mbps and mbps.isdigit():
                        # Curl parancs: csak status code
                        result = relay.cmd(
                            f'curl -k -X POST "https://10.0.0.2:4443/rate_limit?mbps={mbps}" '
                            f'-w "%{{http_code}}" -o /dev/null -s'
                        )

                        status_code = result.strip()
                        timestamp = time.strftime("%H:%M:%S")

                        if status_code == "200":
                            msg = f"[{timestamp}] {mbps} Mbps - Sikerült beállítani\n"
                        else:
                            msg = f"[{timestamp}] {mbps} Mbps - Hiba (HTTP {status_code})\n"

                        # Konzolra és fájlba is
                        print(msg, end='', flush=True)
                        log.write(msg)
                        log.flush()

                        time.sleep(1)

            final_msg = f"\nBandwidth controller befejezve - {time.strftime('%H:%M:%S')}\n"
            print(final_msg, flush=True)
            log.write(final_msg)

    except Exception as e:
        error_msg = f'Bandwidth controller hiba: {e}\n'
        print(error_msg, flush=True)

def start(bw_pr=500.0, bw_bottleneck=500.0, bw_sr=500.0, delay_ms='10ms', loss=0.0, bandwidth_file=None):
    setLogLevel('info')
    net = Mininet(controller=OVSController, link=TCLink, autoSetMacs=True)

    s1 = net.addSwitch('s1')
    c0 = net.addController('c0')

    pub   = net.addHost('pub',   ip='10.0.0.1/24')
    relay = net.addHost('relay', ip='10.0.0.2/24')
    sub   = net.addHost('sub',   ip='10.0.0.3/24')

    # Linkek
    net.addLink(pub,   s1, bw=bw_pr,         delay=delay_ms, loss=loss, max_queue_size=400)
    net.addLink(relay, s1, bw=bw_bottleneck, delay=delay_ms, loss=loss, max_queue_size=400)
    net.addLink(sub,   s1, bw=bw_sr,         delay=delay_ms, loss=loss, max_queue_size=400)

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

    # Bandwidth controller indítása háttérben, ha meg van adva fájl
    if bandwidth_file and os.path.exists(bandwidth_file):
        controller_thread = threading.Thread(
            target=bandwidth_controller,
            args=(net, bandwidth_file),
            daemon=True
        )
        controller_thread.start()
        info(f'*** Bandwidth controller elindítva: {bandwidth_file}\n')
        info(f'*** Log követése: tail -f bandwidth_controller.log\n')

    CLI(net)
    net.stop()

if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--bw-pr', type=float, default=500.0, help='pub–switch bw (Mbit/s)')
    parser.add_argument('--bw-bottleneck', type=float, default=500.0, help='relay–switch bw (Mbit/s)')
    parser.add_argument('--bw-sr', type=float, default=500.0, help='sub–switch bw (Mbit/s)')
    parser.add_argument('--delay', default='10ms', help='link delay (e.g. 50ms)')
    parser.add_argument('--loss', type=float, default=0.0, help='packet loss percent')
    parser.add_argument('--bandwidth-file', type=str, default=None, help='Bandwidth értékek fájlja (pl. param.txt)')
    args = parser.parse_args()
    start(args.bw_pr, args.bw_bottleneck, args.bw_sr, args.delay, args.loss, args.bandwidth_file)
