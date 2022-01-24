# rproxy

## what is rproxy 
rproxy is a rotate proxy for crawler which can call the proxy of proxy.
it build a pool of proxies and you can set the address as proxy of your application 

## build and compile 
cargo build --release

## run

### preparement
before run the rproxy you need to install redis-server and crawl proxies in your pool

hkeys proxy:pool:http

1.1.1.9:8989

2.2.2.9:9090

### script to add ana check proxies

crawl:

    crawl.py http $api -e 10 -m 120 
    crawl.py socks5 $api -e 10 -m 120

check:

    check.py http -c 120
    check.py socks5 -c 120

### start rproxy
after that you can start rproxy 
```shell script
 ./rproxy  -r "redis://:xxxx@127.0.0.1:6379"
```
#### test

```shell script
curl -x http://127.0.0.1:9099 https://icanhazip.com/
curl -x socks5://127.0.0.9089 https://icanhazip.com/
```

# roadmap

1. add authentication for http and socks5

2. add http-tunnel

