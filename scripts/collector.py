"""Versioned supplier worker. Rust owns source configuration, admission and routing (Python 3.11+)."""
import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import ipaddress
import json
import logging
import os
from pathlib import Path
import re
import threading
import time
import tomllib
from urllib.parse import urlsplit
import warnings

import requests

MAX_RESPONSE = 1024 * 1024
MAX_ACK_RESPONSE = 64 * 1024
MAX_ADDRESSES = 4096
MAX_SOURCES = 1000
BATCH_SIZE = 256


class ConfigurationError(ValueError):
    """A value-free configuration diagnostic safe to log."""


class EnvironmentMissing(ConfigurationError):
    """A local environment reference could not be resolved."""


class CollectionError(Exception):
    """A safe category, never a supplier URL, credential or response body."""


class WorkerAccessError(CollectionError):
    """Missing or forbidden worker authorization requires operator intervention."""


class StaleResult(CollectionError):
    """The server rejected this revision/generation; do not retry it."""


def expand(value):
    if isinstance(value, dict):
        return {key: expand(item) for key, item in value.items()}
    if isinstance(value, list):
        return [expand(item) for item in value]
    if isinstance(value, str):
        def variable(match):
            value = os.environ.get(match.group(1))
            if not value:
                raise EnvironmentMissing('required local environment variable is missing')
            return value
        return re.sub(r'\$\{([A-Za-z_][A-Za-z0-9_]*)\}', variable, value)
    return value


def keys(value, allowed, label):
    if not isinstance(value, dict) or set(value) - set(allowed):
        raise ConfigurationError(f'unknown or invalid {label} fields')


def positive(value, name, maximum):
    if type(value) is not int or not 1 <= value <= maximum:
        raise ConfigurationError(f'{name} must be an integer in 1..{maximum}')


def http_url(value):
    try:
        if not isinstance(value, str):
            raise ValueError
        parsed = urlsplit(value)
        if (parsed.scheme not in ('http', 'https') or not parsed.hostname or parsed.username is not None
                or parsed.password is not None or parsed.fragment or parsed.port == 0
                or any(ord(char) <= 32 or ord(char) == 127 for char in value)):
            raise ValueError
    except ValueError:
        raise ConfigurationError('HTTP(S) URL without embedded credentials is required') from None
    return parsed


def resolve_ca(config, directory):
    if 'ca_certificate' in config:
        if not isinstance(config['ca_certificate'], str) or not config['ca_certificate']:
            raise ConfigurationError('ca_certificate must be a nonempty path string')
        try:
            config['ca_certificate'] = str((directory / config['ca_certificate']).resolve())
        except (ValueError, OSError):
            raise ConfigurationError('invalid ca_certificate path') from None


def load_config(path):
    path = Path(path)
    try:
        with path.open('rb') as source:
            config = tomllib.load(source)
    except (OSError, ValueError):
        raise ConfigurationError('cannot read bootstrap TOML; check file and syntax') from None
    keys(config, ('rproxy', 'providers', 'workers', 'poll_seconds'), 'configuration')
    legacy = config.get('providers', [])
    if not isinstance(legacy, list) or legacy:
        raise ConfigurationError('legacy providers are not a runtime authority; use rproxy-cli source import explicitly')
    workers = config.get('workers', 4)
    poll_seconds = config.get('poll_seconds', 3)
    positive(workers, 'workers', 32)
    positive(poll_seconds, 'poll_seconds', 300)
    remote = config.get('rproxy', {})
    keys(remote, ('url', 'token_env', 'timeout_seconds', 'ca_certificate'), 'rproxy')
    remote = {'url': 'http://127.0.0.1:9100', 'token_env': 'RPROXY_WORKER_TOKEN',
              'timeout_seconds': 15, **expand(remote)}
    parsed = http_url(remote['url'])
    if parsed.path not in ('', '/') or parsed.query:
        raise ConfigurationError('rproxy URL must contain only scheme, host and port')
    if not isinstance(remote['token_env'], str) or not re.fullmatch(r'[A-Za-z_][A-Za-z0-9_]*', remote['token_env']):
        raise ConfigurationError('rproxy token_env must be an environment variable name')
    positive(remote['timeout_seconds'], 'rproxy timeout', 300)
    directory = path.parent.resolve()
    resolve_ca(remote, directory)
    return {'rproxy': remote, 'workers': workers, 'poll_seconds': poll_seconds, 'config_dir': directory}


def prepare_provider(raw, directory):
    keys(raw, ('name', 'enabled', 'url', 'protocol', 'method', 'params', 'headers', 'json', 'response',
               'interval_seconds', 'timeout_seconds', 'attempts', 'ca_certificate', 'insecure'), 'provider')
    provider = {'protocol': 'http', 'method': 'GET', 'interval_seconds': 60, 'timeout_seconds': 10,
                'attempts': 3, 'response': {'format': 'text'}, **expand(raw)}
    http_url(provider.get('url'))
    if provider['protocol'] not in ('http', 'socks5') or provider['method'] not in ('GET', 'POST'):
        raise ConfigurationError('provider requires http/socks5 protocol and GET/POST method')
    if type(provider.get('insecure', False)) is not bool:
        raise ConfigurationError('provider insecure must be boolean')
    for field, maximum in [('interval_seconds', 86400), ('timeout_seconds', 300), ('attempts', 5)]:
        positive(provider[field], field, maximum)
    for field in ('params', 'headers', 'json'):
        if field in provider and not isinstance(provider[field], dict):
            raise ConfigurationError(f'provider {field} must be a table')
    if any(not isinstance(key, str) or not isinstance(value, str)
           for key, value in provider.get('headers', {}).items()):
        raise ConfigurationError('provider headers must contain strings')
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
    if any(field.endswith('_path') and not isinstance(value, str) for field, value in mapping.items()):
        raise ConfigurationError('response paths must be strings')
    resolve_ca(provider, Path(directory))
    return provider


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
    except (KeyError, IndexError, TypeError, ValueError, RecursionError):
        raise ConfigurationError('supplier response does not match configured address mapping') from None


def session():
    client = requests.Session()
    client.trust_env = False
    return client


def read_response(response, maximum, deadline):
    content = bytearray()
    # read1 returns available bytes without waiting to fill a large buffered chunk.
    # Older urllib3 releases lack it; single-byte iteration preserves the deadline there.
    read1 = getattr(response.raw, 'read1', None)
    chunks = iter(lambda: read1(8192, decode_content=True), b'') if read1 else response.iter_content(1)
    for chunk in chunks:
        if time.monotonic() > deadline or len(content) + len(chunk) > maximum:
            raise CollectionError('response limit exceeded')
        content.extend(chunk)
    return content.decode('utf-8-sig')


def fetch(provider):
    for attempt in range(provider['attempts']):
        try:
            deadline = time.monotonic() + provider['timeout_seconds']
            # urllib3's insecure warning includes the supplier hostname, so never emit it.
            with warnings.catch_warnings():
                warnings.simplefilter('ignore', requests.packages.urllib3.exceptions.InsecureRequestWarning)
                with session() as client, client.request(provider['method'], provider['url'],
                        params=provider.get('params'), headers=provider.get('headers'), json=provider.get('json'),
                        timeout=provider['timeout_seconds'],
                        verify=False if provider.get('insecure') else provider.get('ca_certificate', True),
                        allow_redirects=False, stream=True) as response:
                    if response.status_code != 200:
                        raise CollectionError('fetch-failed')
                    body = read_response(response, MAX_RESPONSE, deadline)
        except UnicodeError:
            raise CollectionError('mapping-failed') from None
        except (requests.RequestException, requests.packages.urllib3.exceptions.HTTPError, OSError, ValueError, CollectionError):
            if attempt + 1 == provider['attempts']:
                raise CollectionError('fetch-failed') from None
            time.sleep(min(0.25 * 2 ** attempt, 2))
            continue
        try:
            return addresses(body, provider['response'])
        except ConfigurationError:
            raise CollectionError('mapping-failed') from None
    return []


def worker_token(remote):
    token = os.environ.get(remote['token_env'])
    if not token:
        raise ConfigurationError('worker token environment variable is missing')
    return token


def worker_request(remote, method, path, payload=None):
    token = worker_token(remote)
    try:
        deadline = time.monotonic() + remote['timeout_seconds']
        with session() as client, client.request(method, remote['url'].rstrip('/') + path,
                headers={'Authorization': 'Bearer ' + token}, json=payload, timeout=remote['timeout_seconds'],
                verify=remote.get('ca_certificate', True), allow_redirects=False, stream=True) as response:
            if response.status_code in (401, 403):
                raise WorkerAccessError('worker authorization rejected')
            if response.status_code == 409:
                raise StaleResult('stale result rejected')
            if response.status_code != (200 if method == 'GET' else 202):
                raise CollectionError('rproxy unavailable or request rejected')
            maximum = MAX_RESPONSE if method == 'GET' else MAX_ACK_RESPONSE
            return json.loads(read_response(response, maximum, deadline))
    except (requests.RequestException, requests.packages.urllib3.exceptions.HTTPError, OSError, ValueError, RecursionError):
        raise CollectionError('rproxy transport or response failed') from None


def fetch_plan(remote):
    plan = worker_request(remote, 'GET', '/v1/collector/plan')
    try:
        if not isinstance(plan, dict) or plan.get('version') != '0.2.3':
            raise ValueError
        items = plan['sources']
        if not isinstance(items, list) or len(items) > MAX_SOURCES:
            raise ValueError
        sources = {}
        for source in items:
            if (not isinstance(source, dict) or not isinstance(source.get('id'), str)
                    or not re.fullmatch(r's_[A-Za-z0-9_-]{1,128}', source['id'])
                    or source['id'] in sources or not isinstance(source.get('name'), str)
                    or not isinstance(source.get('config'), dict)):
                raise ValueError
            for field, minimum in [('version', 1), ('run_generation', 0)]:
                if type(source.get(field)) is not int or not minimum <= source[field] <= 2 ** 64 - 1:
                    raise ValueError
            sources[source['id']] = source
        return sources
    except (KeyError, TypeError, ValueError):
        raise CollectionError('invalid collector plan') from None


def submit(remote, source, candidates, error=None):
    accepted = 0
    for start in range(0, max(1, len(candidates)), BATCH_SIZE):
        batch = candidates[start:start + BATCH_SIZE]
        result = worker_request(remote, 'POST', '/v1/collector/results', {
            'source_id': source['id'], 'version': source['version'], 'run_generation': source['run_generation'],
            'addresses': batch, 'complete': start + BATCH_SIZE >= len(candidates), 'error': error})
        if (not isinstance(result, dict) or type(result.get('accepted')) is not int
                or not 0 <= result['accepted'] <= len(batch)):
            raise CollectionError('invalid result acknowledgement')
        accepted += result['accepted']
    return accepted


def collect(config, source):
    error = None
    candidates = []
    try:
        provider = prepare_provider(source['config'], config['config_dir'])
    except EnvironmentMissing:
        error = 'env-missing'
    except ConfigurationError:
        error = 'mapping-failed'
    else:
        try:
            candidates = fetch(provider)
        except CollectionError as failure:
            error = str(failure)
    accepted = submit(config['rproxy'], source, candidates, error)
    if error:
        raise CollectionError(error)
    return accepted


def run_once(config):
    worker_token(config['rproxy'])
    sources = fetch_plan(config['rproxy'])
    total, failed = 0, False
    with ThreadPoolExecutor(max_workers=config['workers']) as workers:
        jobs = {workers.submit(collect, config, source): source['id'] for source in sources.values()}
        for future in as_completed(jobs):
            try:
                total += future.result()
            except WorkerAccessError:
                raise
            except CollectionError as error:
                logging.warning('source_id=%s %s', jobs[future], error)
                failed = True
    if failed:
        raise CollectionError('one or more source runs failed')
    return total


def revision(source):
    return source['version'], source['run_generation']


class Worker:
    """One bounded executor and one in-flight job per immutable source ID."""

    def __init__(self, config):
        worker_token(config['rproxy'])
        self.config = config
        self.sources = {}
        self.due = {}
        self.pending = {}
        self.next_poll = 0
        self.workers = ThreadPoolExecutor(max_workers=config['workers'])

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.workers.shutdown(wait=True, cancel_futures=True)

    def tick(self, now=None):
        now = time.monotonic() if now is None else now
        if now >= self.next_poll:
            self.next_poll = now + self.config['poll_seconds']
            try:
                sources = fetch_plan(self.config['rproxy'])
            except WorkerAccessError:
                raise
            except CollectionError:
                logging.warning('collector plan unavailable or invalid; retrying at next poll')
            else:
                self.due = {identity: self.due.get(identity, now)
                            if identity in self.sources and revision(source) == revision(self.sources[identity])
                            else now for identity, source in sources.items()}
                self.sources = sources
        for identity, (future, source) in list(self.pending.items()):
            if not future.done():
                continue
            stale = False
            try:
                logging.info('source_id=%s accepted=%d (pending Rust verification)', identity, future.result())
            except WorkerAccessError:
                raise
            except StaleResult:
                stale = True
                logging.info('source_id=%s stale result discarded; waiting for a new revision or generation', identity)
            except CollectionError as error:
                logging.warning('source_id=%s %s; next run at source interval', identity, error)
            except Exception:
                logging.warning('source_id=%s collection failed; next run at source interval', identity)
            del self.pending[identity]
            current = self.sources.get(identity)
            if current is not None and revision(current) == revision(source):
                interval = source['config'].get('interval_seconds', 60)
                if type(interval) is not int or not 1 <= interval <= 86400:
                    interval = 60
                self.due[identity] = float('inf') if stale else now + interval
        # Avoid queuing an unbounded backlog: let new plans replace unscheduled jobs.
        for identity, source in self.sources.items():
            if len(self.pending) >= self.config['workers']:
                break
            if identity not in self.pending and now >= self.due[identity]:
                self.pending[identity] = (self.workers.submit(collect, self.config, source), source)


def run(config, stop=None):
    stop = stop if stop is not None else threading.Event()
    with Worker(config) as worker:
        while not stop.is_set():
            worker.tick()
            stop.wait(0.2)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', type=Path, required=True)
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument('--once', action='store_true')
    modes.add_argument('--check-config', action='store_true')
    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO, format='%(asctime)s %(levelname)s %(message)s')
    try:
        config = load_config(args.config)
        if args.check_config:
            print('Bootstrap configuration valid (sources are managed online)')
        elif args.once:
            print(f'Accepted {run_once(config)} candidates (pending Rust verification)')
        else:
            run(config)
    except KeyboardInterrupt:
        pass
    except (ConfigurationError, CollectionError) as error:
        logging.error('%s', error)
        raise SystemExit(1) from None
    except Exception:
        # Third-party exception messages may contain URLs or supplier secrets.
        logging.error('collector failed; check configuration, environment and connectivity')
        raise SystemExit(1) from None


if __name__ == '__main__':
    main()
