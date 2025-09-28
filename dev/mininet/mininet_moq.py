#!/usr/bin/env python3
from mininet.net import Mininet
from mininet.node import OVSController, Host
from mininet.link import TCLink
from mininet.log import setLogLevel, info
from mininet.cli import CLI
import argparse
import os

def start(bw_pr=50, bw_bottleneck=0.5, bw_sr=50, delay_ms='50ms', loss=0.0):
    setLogLevel('info')
    net = Mininet(controller=OVSController, link=TCLink, autoSetMacs=True)

    s1 = net.addSwitch('s1')
    c0 = net.addController('c0')

    pub   = net.addHost('pub',   ip='10.0.0.1/24')
    relay = net.addHost('relay', ip='10.0.0.2/24')
    sub   = net.addHost('sub',   ip='10.0.0.3/24')

    # Linkek: pub–relay irányban nagyobb sávszél, relay–sub a szűk keresztmetszet
    net.addLink(pub,   s1, bw=bw_pr,         delay=delay_ms, loss=loss, max_queue_size=100)
    net.addLink(relay, s1, bw=bw_bottleneck, delay=delay_ms, loss=loss, max_queue_size=50)
    net.addLink(sub,   s1, bw=bw_sr,         delay=delay_ms, loss=loss, max_queue_size=100)

    net.start()

    # Munka könyvtár: a repo gyökere (ugyanaz fs minden hostnak)
    root = os.path.abspath(os.path.join(os.path.dirname(__file__), '..', '..'))
    bins = os.path.join(root, 'target', 'release')

    # Tanúsítvány (ha még nincs)
    if not os.path.exists(os.path.join(root, 'dev', 'localhost.crt')):
        os.system(f'cd {root} && ./dev/cert')

    # Relay indítása (10.0.0.2:4443)
    relay_cmd = (
        f'cd {root} && '
        f'RUST_LOG=info '
        f'{bins}/moq-relay-ietf '
        f'--bind 10.0.0.2:4443 '
        f'--tls-cert dev/localhost.crt --tls-key dev/localhost.key '
        f'> relay.log 2>&1 &'
    )
    info(f'*** start relay: {relay_cmd}\n')
    relay.cmd(relay_cmd)

    # Publisher: csővezeték a tesztfájlra, vagy ffmpeg. Itt a dev/bbb.fmp4-et használjuk.
    pub_input = os.path.join(root, 'dev', 'bbb.fmp4')
    if not os.path.exists(pub_input):
        info('*** dev/bbb.fmp4 hiányzik, futtasd előtte ./dev/pub a hoston a letöltéshez/konvertáláshoz\n')
    pub_cmd = (
        f'cd {root} && '
        f'RUST_LOG=moq_transport=debug,moq_pub=info '
        f'cat {pub_input} | {bins}/moq-pub '
        f'--tls-disable-verify '
        f'--name bbb '
        f'https://10.0.0.2:4443 '
        f'> pub.log 2>&1 &'
    )
    info(f'*** start publisher: {pub_cmd}\n')
    pub.cmd(pub_cmd)

    # Subscriber: csatlakozik a relay-hez (IP alapú URL), TLS verify off
    sub_cmd = (
        f'cd {root} && '
        f'RUST_LOG=moq_transport=debug,moq_sub=info '
        f'{bins}/moq-sub '
        f'--tls-disable-verify '
        f'--name bbb '
        f'https://10.0.0.2:4443/bbb '
        f'> sub.log 2>&1 &'
    )
    info(f'*** start subscriber: {sub_cmd}\n')
    sub.cmd(sub_cmd)

    info('*** fut: relay.log / pub.log / sub.log a repo gyökerében\n')
    info('*** Mininet CLI: pl. link újrakonfigurálás: link s1-relay bw 0.3 delay 80ms\n')
    CLI(net)

    net.stop()

if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--bw-pr', type=float, default=50.0, help='pub–switch bw (Mbit/s)')
    parser.add_argument('--bw-bottleneck', type=float, default=0.5, help='relay–switch bw (Mbit/s)')
    parser.add_argument('--bw-sr', type=float, default=50.0, help='sub–switch bw (Mbit/s)')
    parser.add_argument('--delay', default='50ms', help='link delay (e.g. 50ms)')
    parser.add_argument('--loss', type=float, default=0.0, help='packet loss percent')
    args = parser.parse_args()
    start(args.bw_pr, args.bw_bottleneck, args.bw_sr, args.delay, args.loss)