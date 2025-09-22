#!/usr/bin/env python3
from mininet.topo import Topo
from mininet.net import Mininet
from mininet.node import OVSController
from mininet.link import TCLink
from mininet.log import setLogLevel, info
from time import sleep

BIN_DIR = "/tmp/moq"
RELAY_PORT = 4443
BROADCAST_NAME = "bbb"

class MoqLineTopo(Topo):
    def build(self, **_opts):
        hR = self.addHost('hR')
        hP = self.addHost('hP')
        hS = self.addHost('hS1')
        # Relay <-> Publisher (alacsony késleltetés, jó sávszél)
        self.addLink(hR, hP, cls=TCLink, bw=50, delay='20ms', loss=0)
        # Publisher <-> Subscriber (szűkebb link + loss)
        self.addLink(hP, hS, cls=TCLink, bw=5, delay='50ms', loss=2)

def start_relay(host):
    cmd = f"{BIN_DIR}/moq-relay-ietf --bind [::]:{RELAY_PORT} --tls-cert {BIN_DIR}/dev/localhost.crt --tls-key {BIN_DIR}/dev/localhost.key --bandwidth-monitoring 5 --initial-rtt-ms 50 --rate-limit-bps 50000000 --dev &> relay.log &"
    host.cmd(cmd)

def start_publisher(host):
    # Feltételezzük hogy van egy bemeneti fmp4 (pl. dev/bbb_fix.fmp4) – másold be BIN_DIR-be
    url = f"https://hR:{RELAY_PORT}"
    cmd = (
        f"ffmpeg -hide_banner -v quiet -re -stream_loop -1 -i {BIN_DIR}/dev/bbb_fix.fmp4 "
        "-c copy -f mp4 -movflags cmaf+separate_moof+delay_moov+skip_trailer+frag_every_frame - | "
        f"{BIN_DIR}/moq-pub --name {BROADCAST_NAME} {url} "
        f"--bandwidth-monitoring --rate-limit-bps 20000000 --initial-rtt-ms 50 &> pub.log &"
    )
    host.cmd(cmd)

def start_subscriber(host):
    url = f"https://hR:{RELAY_PORT}/{BROADCAST_NAME}"
    cmd = f"{BIN_DIR}/moq-sub --name {BROADCAST_NAME} {url} --rate-limit-bps 15000000 --initial-rtt-ms 50 &> sub.log &"
    host.cmd(cmd)

def dump_logs(host, label):
    info(f"\n--- {label} relay.log ---\n")
    info(host.cmd("tail -n 20 relay.log || true"))

def run():
    topo = MoqLineTopo()
    net = Mininet(topo=topo, controller=OVSController, link=TCLink, autoStaticArp=True)
    net.start()
    hR, hP, hS = net.get('hR', 'hP', 'hS1')

    info("*** Indít relay\n")
    start_relay(hR)
    sleep(2)

    info("*** Indít publisher\n")
    start_publisher(hP)
    sleep(3)

    info("*** Indít subscriber\n")
    start_subscriber(hS)
    sleep(8)

    info("*** Gyors ellenőrzés (UDP port)\n")
    info(hR.cmd(f"ss -u -l | grep {RELAY_PORT} || true"))

    info("*** Log minták\n")
    info(hR.cmd("grep -i subscribe relay.log | tail -n 5 || true"))
    info(hP.cmd("grep -i 'Rate limit' pub.log | tail -n 2 || true"))
    info(hS.cmd("grep -i 'Error' sub.log | tail -n 5 || true"))

    info("\n*** Interaktív Mininet CLI (exit: CTRL-D vagy quit)\n")
    from mininet.cli import CLI
    CLI(net)

    net.stop()

if __name__ == "__main__":
    setLogLevel('info')
    run()
