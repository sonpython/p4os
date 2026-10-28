#!/usr/bin/env python3
"""Minimal ONVIF client for the Imou camera: read/set imaging settings so the
backlit ESP32 screen is not over-exposed.

Usage: cam_onvif.py get | set KEY=VALUE ... | options
  e.g. cam_onvif.py set Brightness=30 Exposure.Mode=MANUAL
Credentials: ~/.config/p4os/imou (user admin). Camera: $P4OS_CAM.
"""
import base64
import datetime as dt
import hashlib
import os
import re
import sys
import urllib.request
from pathlib import Path

IP = os.environ.get("P4OS_CAM", "192.168.88.33")
USER = "admin"
PASSWORD = (Path.home() / ".config/p4os/imou").read_text().strip()


def soap(url: str, body: str, auth: bool = True) -> str:
    header = ""
    if auth:
        nonce = os.urandom(16)
        created = dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        digest = base64.b64encode(hashlib.sha1(nonce + created.encode() + PASSWORD.encode()).digest()).decode()
        header = f"""<s:Header><Security s:mustUnderstand="1" xmlns="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd"><UsernameToken><Username>{USER}</Username><Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{digest}</Password><Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">{base64.b64encode(nonce).decode()}</Nonce><Created xmlns="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd">{created}</Created></UsernameToken></Security></s:Header>"""
    env = f'<?xml version="1.0"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tt="http://www.onvif.org/ver10/schema">{header}<s:Body>{body}</s:Body></s:Envelope>'
    req = urllib.request.Request(url, env.encode(), {"Content-Type": "application/soap+xml"})
    try:
        return urllib.request.urlopen(req, timeout=10).read().decode()
    except urllib.error.HTTPError as e:
        return e.read().decode()


def video_source_token() -> str:
    r = soap(f"http://{IP}/onvif/media_service", '<GetVideoSources xmlns="http://www.onvif.org/ver10/media/wsdl"/>')
    m = re.search(r'VideoSources[^>]*token="([^"]+)"', r)
    if not m:
        raise SystemExit("no video source: " + r[:300])
    return m.group(1)


def imaging_url() -> str:
    r = soap(f"http://{IP}/onvif/device_service",
             '<GetCapabilities xmlns="http://www.onvif.org/ver10/device/wsdl"><Category>Imaging</Category></GetCapabilities>')
    m = re.search(r"<tt:XAddr>([^<]+)</tt:XAddr>", r)
    return m.group(1) if m else f"http://{IP}/onvif/imaging_service"


def strip(xml: str) -> str:
    body = re.search(r"<s:Body>(.*)</s:Body>", xml, re.S)
    return re.sub(r"<(/?)[a-zA-Z]+:", r"<\1", body.group(1) if body else xml)


def main() -> None:
    cmd = sys.argv[1] if len(sys.argv) > 1 else "get"
    token = video_source_token()
    url = imaging_url()
    ns = 'xmlns="http://www.onvif.org/ver20/imaging/wsdl"'
    if cmd == "get":
        print(strip(soap(url, f"<GetImagingSettings {ns}><VideoSourceToken>{token}</VideoSourceToken></GetImagingSettings>")))
    elif cmd == "options":
        print(strip(soap(url, f"<GetOptions {ns}><VideoSourceToken>{token}</VideoSourceToken></GetOptions>")))
    elif cmd == "set":
        # Build a nested ImagingSettings element from dotted KEY=VALUE pairs.
        tree: dict = {}
        for kv in sys.argv[2:]:
            k, v = kv.split("=", 1)
            node = tree
            parts = k.split(".")
            for p in parts[:-1]:
                node = node.setdefault(p, {})
            node[parts[-1]] = v

        def render(d: dict) -> str:
            return "".join(f"<tt:{k}>{render(v) if isinstance(v, dict) else v}</tt:{k}>" for k, v in d.items())

        body = (f"<SetImagingSettings {ns}><VideoSourceToken>{token}</VideoSourceToken>"
                f"<ImagingSettings>{render(tree)}</ImagingSettings><ForcePersistence>true</ForcePersistence></SetImagingSettings>")
        print(strip(soap(url, body)))
    else:
        print(__doc__)


if __name__ == "__main__":
    main()
