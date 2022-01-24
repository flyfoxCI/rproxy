import json
import requests


def get_xdaili_proxy(url):
    proxies = []
    try:
        resp = requests.get(url, timeout=30000)
        if resp.status_code in [200, 201, 203, 204]:
            o = json.loads(resp.text)
            if o.get("ERRORCODE") == "0":
                result = o["RESULT"]
                proxies = []
                for r in result:
                    p = r.get("ip") + ":" + r.get("port")
                    proxies.append(p)

    except Exception as e:
        print(e)
    return proxies


def get_shenlong_proxy(url):
    proxies = []
    try:
        resp = requests.get(url, timeout=30000)
        if resp.status_code in [200, 201, 203, 204]:
            o = json.loads(resp.text)
            if o.get("code") == 200:
                result = o["data"]
                proxies = []
                for r in result:
                    p = r.get("ip") + ":" + str(r.get("port"))
                    proxies.append(p)
    except Exception as e:
        print(e)
    return proxies
