#!/usr/bin/env python3
"""One bounded, text-only DHCP/RTSP connection capture on a new P2P interface.

Run as the desktop user in a visible terminal. Only tcpdump is elevated, and
it drops privileges after opening the interface. Never uses monitor mode,
promiscuous mode, an all-interfaces capture, or a raw packet savefile.
"""

import argparse
from contextlib import contextmanager
from datetime import datetime
import os
from pathlib import Path
import pwd
import re
import shutil
import socket
import subprocess
import tempfile
import time


FILTER = (
    "(ip and udp and (port 67 or port 68)) or "
    "(ip and tcp port 7236 and "
    "(tcp[tcpflags] & (tcp-syn|tcp-fin|tcp-rst) != 0) and "
    "((ip[2:2] - ((ip[0] & 0xf) << 2)) - ((tcp[12] & 0xf0) >> 2) = 0))"
)


def p2p_name(name, prefix):
    return (name.startswith(prefix) and name[len(prefix):].isdigit()
            and re.fullmatch(r"[A-Za-z0-9_.-]{1,15}", name) is not None)


def candidates(prefix, require_up=False, sys_net=Path("/sys/class/net")):
    result = []
    for entry in sys_net.iterdir():
        if not p2p_name(entry.name, prefix):
            continue
        try:
            if not require_up or int((entry / "flags").read_text().strip(), 16) & 1:
                result.append(entry.name)
        except FileNotFoundError:
            pass  # The short-lived group was removed between link events.
    return result


def capture_command(interface, username, tcpdump):
    if not re.fullmatch(r"p2p-[A-Za-z0-9_.-]{1,11}", interface):
        raise ValueError("Solo se permite una interfaz Wi-Fi Direct explícita")
    return ["sudo", "-n", "timeout", "--signal=INT", "--kill-after=3s", "65s",
            tcpdump, "-i", interface, "-p", "-n", "-e", "-tttt", "-vv", "-l",
            "--immediate-mode", "-s", "576", "-c", "200", "-Z", username,
            "--", FILTER]


@contextmanager
def diagnostic_log(path):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8", buffering=1) as log:
        def report(text):
            # Persist first: a closed terminal must not lose the last message.
            text = "".join(c if c.isprintable() or c in "\n\t" else "?" for c in text)
            text = text if text.endswith("\n") else text + "\n"
            log.write(text)
            print(text, end="", flush=True)
        try:
            yield report
        except BaseException as error:
            try:
                report(f"Preparación/captura interrumpida: {type(error).__name__}: {error}")
            except BrokenPipeError:
                pass
            raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prefix", default="p2p-wlp0s20-",
                        help="Prefijo del grupo P2P de este adaptador; no admite interfaces LAN")
    args = parser.parse_args()
    if not re.fullmatch(r"p2p-[A-Za-z0-9_.-]+-", args.prefix):
        parser.error("Prefijo P2P inválido")
    if os.geteuid() == 0:
        parser.error("Ejecutar como usuario normal: el programa solicita sudo solo para tcpdump")
    tcpdump = shutil.which("tcpdump")
    if not tcpdump:
        parser.error("tcpdump todavía no está instalado")
    os.umask(0o077)
    directory = Path(tempfile.mkdtemp(prefix="miracast-captura-", dir="/tmp/opencode"))
    log_path = directory / "captura.txt"
    username = pwd.getpwuid(os.getuid()).pw_name

    with diagnostic_log(log_path) as report:
        report(f"Inicio: {datetime.now().astimezone().isoformat()}")
        report(f"Registro privado: {log_path}")
        report("Solo DHCP y cabeceras TCP 7236 sin datos. Sin PCAP, modo monitor ni cambios UFW.")
        inactive = sorted(set(candidates(args.prefix)) - set(candidates(args.prefix, require_up=True)))
        if inactive:
            report("Interfaces residuales DOWN ignoradas: " + ", ".join(inactive))
        # Validate the BPF without opening any network interface or using sudo.
        compiled = subprocess.run([tcpdump, "-d", "-y", "EN10MB", "--", FILTER],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        if compiled.returncode:
            raise RuntimeError(f"Filtro de captura inválido: {compiled.stderr.strip()}")
        report("Primero pulsa Detener en el plugin si hay un intento activo.")
        report("Pon la Samsung en Compartir pantalla / Miracast.")
        report("Esperando tu confirmación: pulsa Enter aquí. No conectes desde el plugin hasta ver ARMADO.")
        input()
        report("Solicitando autorización de captura; introduce tu contraseña si se pide.")
        subprocess.run(["sudo", "-v"], check=True)
        # RTMGRP_LINK is a read-only kernel link-event subscription. The socket
        # is opened before publishing ARMED to avoid missing a quick hotplug.
        with socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, socket.NETLINK_ROUTE) as links:
            links.bind((0, 1))
            if candidates(args.prefix, require_up=True):
                report("Todavía hay un grupo P2P. Pulsa Detener en el plugin y luego Enter aquí.")
                input()
                if candidates(args.prefix, require_up=True):
                    raise RuntimeError("El grupo anterior sigue activo; no se capturará una sesión comenzada antes de ARMADO")
            report("ARMADO. Plugin: Buscar TV → Samsung → Patrón de prueba → Conectar. Acepta en la TV.")
            report("Se esperará como máximo 90 segundos a que aparezca el grupo; después se capturará hasta 65 segundos.")
            deadline = time.monotonic() + 90
            interface = None
            while time.monotonic() < deadline:
                found = candidates(args.prefix, require_up=True)
                if len(found) > 1:
                    raise RuntimeError("Hay varios grupos P2P; se cancela para no capturar otro enlace")
                if found:
                    interface = found[0]
                    break
                links.settimeout(max(0.01, deadline - time.monotonic()))
                try:
                    links.recv(65536)
                except socket.timeout:
                    break
            if interface is None:
                report("No apareció un grupo P2P nuevo en 90 segundos; no se capturó tráfico.")
                return 1

        report(f"Interfaz exclusiva: {interface}")
        process = subprocess.Popen(capture_command(interface, username, tcpdump),
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                   text=True, encoding="utf-8", errors="replace", bufsize=1)
        output_bytes = 0
        try:
            for line in process.stdout:
                output_bytes += len(line.encode("utf-8"))
                if output_bytes > 1024 * 1024:
                    report("Se alcanzó el límite de 1 MiB de diagnóstico; se cancela la captura.")
                    process.terminate()
                    break
                report(line)
            code = process.wait(timeout=10)
        finally:
            if process.poll() is None:
                try:
                    process.terminate()
                except PermissionError:
                    pass  # The privileged timeout still enforces the 65s cap.
            process.stdout.close()
        report(f"Fin: {datetime.now().astimezone().isoformat()} · salida={code}")
        report(f"Guardado: {log_path}")
        if code == 124:
            report("Captura cerrada al alcanzar el tiempo máximo.")
        return 0 if code in (0, 124) else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        print("\nCaptura cancelada. No se ha modificado la red.", flush=True)
        raise SystemExit(130)
