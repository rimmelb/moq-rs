#!/usr/bin/env python3
import subprocess
import time

# Beállítások
BANDWIDTH_FILE = "/home/user/moq-rs/tools/param.txt"
RELAY_IP = "10.0.0.2"
RELAY_PORT = "4443"
INTERVAL = 1

def send_rate_limit(mbps):
    """Elküldi a rate limit parancsot a relay-nek"""
    cmd = f'relay curl -k -X POST "https://{RELAY_IP}:{RELAY_PORT}/rate_limit?mbps={mbps}"'
    try:
        result = subprocess.run(cmd, shell=True, capture_output=True, text=True)
        print(f"[{time.strftime('%H:%M:%S')}] Sávszélesség beállítva: {mbps} Mbps")
        if result.returncode != 0:
            print(f"  Hiba: {result.stderr}")
    except Exception as e:
        print(f"  Kivétel: {e}")

# Fájl beolvasása és parancsok küldése
with open(BANDWIDTH_FILE, 'r') as f:
    for line in f:
        mbps = line.strip()
        if mbps:  # Üres sorok kihagyása
            send_rate_limit(mbps)
            time.sleep(INTERVAL)

print("Minden sávszélesség érték elküldve!")
