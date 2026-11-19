import asyncio
import json
import logging
import uuid
from datetime import datetime, timezone

import redis

from conf import REDIS_HOST, REDIS_PORT, REDIS_PASSWORD, REDIS_URL


def connection():
    if REDIS_URL:
        return redis.Redis.from_url(REDIS_URL, socket_timeout=10, socket_connect_timeout=5)
    return redis.Redis(host=REDIS_HOST, port=REDIS_PORT, password=REDIS_PASSWORD,
                       socket_timeout=10, socket_connect_timeout=5)


def record(proxy):
    return json.dumps({"private_ip": proxy, "ts": datetime.now(timezone.utc).isoformat(),
                       "generation": uuid.uuid4().hex})


async def check_many(check, proxies, concurrency):
    semaphore = asyncio.Semaphore(concurrency)

    async def one(proxy):
        async with semaphore:
            try:
                return await check(proxy)
            except Exception:
                logging.warning("proxy check failed unexpectedly")
                return False, proxy

    return await asyncio.gather(*(one(proxy) for proxy in proxies))


def safe_run(job):
    try:
        job.run()
    except Exception:
        logging.error("proxy maintenance iteration failed; will retry at next interval")
