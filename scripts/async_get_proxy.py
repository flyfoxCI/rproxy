import ipaddress
import logging

import requests


def _addresses(rows):
    proxies = []
    for row in rows:
        try:
            ip = ipaddress.ip_address(row["ip"])
            port = int(row["port"])
            if not 1 <= port <= 65535:
                continue
            host = f"[{ip}]" if ip.version == 6 else str(ip)
            proxies.append(f"{host}:{port}")
        except (KeyError, TypeError, ValueError):
            continue
    return list(dict.fromkeys(proxies))


def _get(url):
    response = requests.get(url, timeout=(5, 15))
    response.raise_for_status()
    return response.json()


def get_xdaili_proxy(url):
    try:
        data = _get(url)
        return _addresses(data.get("RESULT", [])) if str(data.get("ERRORCODE")) == "0" else []
    except (requests.RequestException, ValueError, TypeError, AttributeError):
        logging.warning("HTTP proxy supplier request failed")
        return []


def get_shenlong_proxy(url):
    try:
        data = _get(url)
        return _addresses(data.get("data", [])) if data.get("code") == 200 else []
    except (requests.RequestException, ValueError, TypeError, AttributeError):
        logging.warning("SOCKS5 proxy supplier request failed")
        return []
