import logging
import os

REDIS_HOST = os.environ.get("RPROXY_REDIS_HOST", "127.0.0.1")
REDIS_PORT = int(os.environ.get("RPROXY_REDIS_PORT", "6379"))
REDIS_PASSWORD = os.environ.get("RPROXY_REDIS_PASSWORD")
REDIS_URL = os.environ.get("RPROXY_REDIS_URL")
REDIS_KEY = os.environ.get("RPROXY_REDIS_KEY", "proxy:pool")
CHECK_URL = os.environ.get("RPROXY_CHECK_URL", "https://api.ipify.org/")
CHECK_CA = os.environ.get("RPROXY_CHECK_CA")
CHECK_INSECURE = os.environ.get("RPROXY_CHECK_INSECURE", "false").lower() == "true"
LOG_LEVEL = logging.INFO
LOG_FORMAT = "%(name)s %(message)s"
