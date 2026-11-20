"""Configurable supplier collector. Rust owns admission, health and routing (Python 3.11+)."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import ipaddress
import json
import logging
import os
from pathlib import Path
import re
import time
import tomllib
from urllib.parse import urlsplit

import requests

MAX_RESPONSE = 1024 * 1024
MAX_ADDRESSES = 4096


class ConfigurationError(ValueError):
    """A value-free configuration diagnostic safe to log."""


class CollectionError(Exception):
    """A safe error message which never embeds supplier URLs, credentials or bodies."""


def expand(value):
    if isinstance(value, dict):
        return {key: expand(item) for key, item in value.items()}
    if isinstance(value, list):
        return [expand(item) for item in value]
    if isinstance(value, str):
        def variable(match):
            name = match.group(1)
            if not os.environ.get(name):
                raise ConfigurationError(f'required environment variable is missing: {name}')
            return os.environ[name]
        return re.sub(r'\$\{([A-Za-z_][A-Za-z0-9_]*)\}', variable, value)
    return value


def keys(value, allowed, label):
    if not isinstance(value, dict) or set(value) - set(allowed):
        raise ConfigurationError(f'unknown or invalid {label} fields')


def positive(value, name, maximum):
    if type(value) is not int or not 1 <= value <= maximum:
        raise ConfigurationError(f'{name} must be an integer in 1..{maximum}')


def http_url(value):
    parsed = urlsplit(value)
    if parsed.scheme not in ('http', 'https') or not parsed.hostname or parsed.username or parsed.password or parsed.fragment:
        raise ConfigurationError('HTTP(S) URL without embedded credentials is required')
    return value


def load_config(path):
    path = Path(path)
    with path.open('rb') as source:
        config = tomllib.load(source)
    keys(config, ('rproxy', 'providers', 'workers'), 'configuration')
    workers = config.get('workers', 4)
    positive(workers, 'workers', 32)
    remote = config.get('rproxy', {})
    keys(remote, ('url', 'token_env', 'timeout_seconds', 'ca_certificate'), 'rproxy')
    remote = {'url': 'http://127.0.0.1:9100', 'token_env': 'RPROXY_ADMIN_TOKEN', 'timeout_seconds': 15, **expand(remote)}
    http_url(remote['url'])
    if urlsplit(remote['url']).path not in ('', '/') or urlsplit(remote['url']).query:
        raise ConfigurationError('rproxy URL must contain only scheme, host and port')
    if not isinstance(remote['token_env'], str) or not remote['token_env']:
        raise ConfigurationError('rproxy token_env is required')
    positive(remote['timeout_seconds'], 'rproxy timeout', 300)
    providers = []
    names = set()
    raw_providers = config.get('providers', [])
    if not isinstance(raw_providers, list):
        raise ConfigurationError('providers must be an array of tables')
    for raw in raw_providers:
        keys(raw, ('name', 'enabled', 'url', 'protocol', 'method', 'params', 'headers', 'json', 'response',
                   'interval_seconds', 'timeout_seconds', 'attempts', 'ca_certificate', 'insecure'), 'provider')
        name = raw.get('name', '')
        if not isinstance(name, str) or not re.fullmatch(r'[A-Za-z0-9._-]{1,64}', name) or name in names:
            raise ConfigurationError('unique provider name of 1..64 ASCII identifier characters is required')
        names.add(name)
        if type(raw.get('enabled', True)) is not bool:
            raise ConfigurationError('provider enabled must be boolean')
        if not raw.get('enabled', True):
            continue
        provider = {'protocol': 'http', 'method': 'GET', 'interval_seconds': 60, 'timeout_seconds': 10,
                    'attempts': 3, 'response': {'format': 'text'}, **expand(raw)}
        http_url(provider.get('url', ''))
        if provider['protocol'] not in ('http', 'socks5') or provider['method'] not in ('GET', 'POST'):
            raise ConfigurationError('provider requires http/socks5 protocol and GET/POST method')
        if type(provider.get('insecure', False)) is not bool:
            raise ConfigurationError('provider insecure must be boolean')
        for field, maximum in [('interval_seconds', 86400), ('timeout_seconds', 300), ('attempts', 5)]:
            positive(provider[field], field, maximum)
        for field in ('params', 'headers', 'json'):
            if field in provider and not isinstance(provider[field], dict):
                raise ConfigurationError(f'provider {field} must be a table')
        if provider['method'] == 'GET' and 'json' in provider:
            raise ConfigurationError('JSON request body requires POST')
        mapping = provider['response']
        keys(mapping, ('format', 'items_path', 'address_path', 'ip_path', 'port_path', 'success_path', 'success_value'), 'response')
        if mapping.get('format', 'text') not in ('text', 'json'):
            raise ConfigurationError('response format must be text or json')
        if ('success_path' in mapping) != ('success_value' in mapping):
            raise ConfigurationError('success_path and success_value must be configured together')
        if ('ip_path' in mapping) != ('port_path' in mapping) or ('ip_path' in mapping and 'address_path' in mapping):
            raise ConfigurationError('use address_path or both ip_path and port_path')
        for field, value in mapping.items():
            if field.endswith('_path') and not isinstance(value, str):
                raise ConfigurationError('response paths must be strings')
        for field in ('ca_certificate',):
            if provider.get(field):
                provider[field] = str((path.parent / provider[field]).resolve())
        providers.append(provider)
    if remote.get('ca_certificate'):
        remote['ca_certificate'] = str((path.parent / remote['ca_certificate']).resolve())
    if providers and not os.environ.get(remote['token_env']):
        raise ConfigurationError(f"required environment variable is missing: {remote['token_env']}")
    return {'rproxy': remote, 'providers': providers, 'workers': workers}


def at(value, path):
    for part in path.split('.') if path else []:
        if isinstance(value, list) and part.isdecimal():
            value = value[int(part)]
        elif isinstance(value, dict):
            value = value[part]
        else:
            raise ConfigurationError('response path does not match payload')
    return value


def address(value):
    if not isinstance(value, str):
        raise ConfigurationError('proxy address must be a string')
    if value.startswith('['):
        match = re.fullmatch(r'\[([^\]]+)\]:(\d+)', value)
        if not match:
            raise ConfigurationError('invalid IPv6 proxy address')
        host, port = match.groups()
    else:
        host, port = value.rsplit(':', 1)
    ip = ipaddress.ip_address(host)
    if not port.isascii() or not port.isdecimal() or not 1 <= int(port) <= 65535 or ip.is_unspecified:
        raise ConfigurationError('invalid proxy port or IP')
    if isinstance(ip, ipaddress.IPv6Address) and ip.scope_id is not None:
        raise ConfigurationError('scoped IP addresses are unsupported')
    host = f'[{ip}]' if ip.version == 6 else str(ip)
    return f'{host}:{int(port)}'


def addresses(body, mapping):
    try:
        if mapping.get('format', 'text') == 'text':
            values = [part for part in re.split(r'[\s,;]+', body.strip()) if part]
        else:
            payload = json.loads(body)
            if 'success_path' in mapping and at(payload, mapping['success_path']) != mapping['success_value']:
                raise ConfigurationError('supplier reported failure')
            items = at(payload, mapping.get('items_path', ''))
            if not isinstance(items, list):
                raise ConfigurationError('response items must be an array')
            values = []
            for item in items:
                if 'ip_path' in mapping:
                    ip = ipaddress.ip_address(at(item, mapping['ip_path']))
                    host = f'[{ip}]' if ip.version == 6 else str(ip)
                    values.append(f"{host}:{at(item, mapping['port_path'])}")
                else:
                    values.append(at(item, mapping.get('address_path', '')))
        if len(values) > MAX_ADDRESSES:
            raise ConfigurationError('too many proxy addresses')
        return list(dict.fromkeys(address(value) for value in values))
    except (KeyError, IndexError, TypeError, ValueError) as error:
        raise ConfigurationError('supplier response does not match configured address mapping') from error


def session():
    client = requests.Session()
    client.trust_env = False
    return client


def fetch(provider):
    for attempt in range(provider['attempts']):
        try:
            with session() as client, client.request(provider['method'], provider['url'],
                    params=provider.get('params'), headers=provider.get('headers'), json=provider.get('json'),
                    timeout=provider['timeout_seconds'], verify=False if provider.get('insecure') else provider.get('ca_certificate', True),
                    allow_redirects=False, stream=True) as response:
                if response.status_code != 200:
                    raise CollectionError('supplier returned an unsuccessful status')
                content = bytearray()
                for chunk in response.iter_content(8192):
                    if len(content) + len(chunk) > MAX_RESPONSE:
                        raise CollectionError('supplier response exceeds size limit')
                    content.extend(chunk)
                return addresses(content.decode('utf-8-sig'), provider['response'])
        except (requests.RequestException, ValueError, CollectionError):
            if attempt + 1 == provider['attempts']:
                raise CollectionError('supplier fetch failed after bounded attempts') from None
            time.sleep(min(0.25 * 2 ** attempt, 2))
    return []


def submit(remote, provider, candidates):
    token = os.environ.get(remote['token_env'])
    if not token:
        raise CollectionError('management token environment variable is missing')
    accepted = 0
    with session() as client:
        for start in range(0, len(candidates), 256):
            try:
                response = client.post(remote['url'].rstrip('/') + '/v1/proxies',
                    headers={'Authorization': 'Bearer ' + token},
                    json={'protocol': provider['protocol'], 'addresses': candidates[start:start + 256], 'source': provider['name']},
                    timeout=remote['timeout_seconds'], verify=remote.get('ca_certificate', True), allow_redirects=False)
                with response:
                    if response.status_code != 202:
                        raise CollectionError('candidate submission rejected; check capacity, readiness and token')
                    accepted += int(response.json()['accepted'])
            except (requests.RequestException, KeyError, TypeError, ValueError):
                raise CollectionError('cannot submit candidates to rproxy') from None
    return accepted


def collect(remote, provider):
    return submit(remote, provider, fetch(provider))


def run_once(config):
    total = 0
    for provider in config['providers']:
        total += collect(config['rproxy'], provider)
    return total


def run(config):
    due = {provider['name']: 0 for provider in config['providers']}
    pending = {}
    with ThreadPoolExecutor(max_workers=config['workers']) as workers:
        while True:
            now = time.monotonic()
            for name, (future, provider) in list(pending.items()):
                if future.done():
                    try:
                        logging.info('provider=%s accepted=%d (pending Rust verification)', name, future.result())
                    except CollectionError as error:
                        logging.warning('provider=%s %s; retrying at next interval', name, error)
                    except Exception:
                        logging.warning('provider=%s collection failed; retrying at next interval', name)
                    due[name] = now + provider['interval_seconds']
                    del pending[name]
            for provider in config['providers']:
                name = provider['name']
                if name not in pending and now >= due[name]:
                    pending[name] = (workers.submit(collect, config['rproxy'], provider), provider)
            time.sleep(1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', type=Path, required=True)
    parser.add_argument('--once', action='store_true')
    parser.add_argument('--check-config', action='store_true')
    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO, format='%(asctime)s %(levelname)s %(message)s')
    try:
        config = load_config(args.config)
        if args.check_config:
            print(f"Configuration valid ({len(config['providers'])} enabled providers)")
        elif args.once:
            print(f"Accepted {run_once(config)} candidates (pending Rust verification)")
        else:
            if not config['providers']:
                logging.info('No enabled providers; collector is idle')
            run(config)
    except KeyboardInterrupt:
        pass
    except (ConfigurationError, CollectionError) as error:
        logging.error('%s', error)
        raise SystemExit(1) from None
    except Exception as error:
        # Third-party exception messages may contain URLs or supplier secrets.
        logging.error('collector failed (%s); check configuration, environment and connectivity', type(error).__name__)
        raise SystemExit(1) from None


if __name__ == '__main__':
    main()
