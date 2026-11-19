local request = cjson.decode(ARGV[1])
local clock = redis.call('TIME')
local now = tonumber(clock[1]) * 1000 + math.floor(tonumber(clock[2]) / 1000)
local raw = redis.call('GET', KEYS[1])
local state = raw and cjson.decode(raw) or {sessions = {}, cursor = 0}

local function save(result)
    redis.call('SET', KEYS[1], cjson.encode(state))
    return cjson.encode(result)
end
local function failure(message)
    return cjson.encode({error = message})
end
local function empty(t) return next(t) == nil end

for name, session in pairs(state.sessions) do
    session.active = session.active or {}
    for token, lease in pairs(session.active) do
        if lease.deadline <= now then session.active[token] = nil end
    end
    if session.releasing and empty(session.active) then state.sessions[name] = nil end
end

local identity = request.identity
local session = state.sessions[identity.session]
if session and session.owner ~= identity.owner then return failure('session belongs to another identity') end

if request.operation == 'release-session' then
    if session then
        session.releasing = true
        session.binding = nil
        if empty(session.active) then state.sessions[identity.session] = nil end
    end
    return save({ok = true})
end

if request.operation == 'release' then
    if session then
        session.active[request.token] = nil
        if session.releasing and empty(session.active) then state.sessions[identity.session] = nil end
    end
    return save({ok = true})
end

if request.operation == 'renew' then
    if not session or not session.active[request.token] then return failure('connection lease expired') end
    session.active[request.token].deadline = now + request.lease_ms
    return save({ok = true})
end

if request.operation == 'fail' then
    redis.call('HSET', KEYS[2], request.proxy_id, now + request.cooldown_ms)
    if session and session.binding and session.binding.id == request.proxy_id then
        session.previous = session.binding
        session.binding = nil
    end
    return save({ok = true})
end

if session and session.releasing then return failure('session is draining') end
if session and session.strategy ~= identity.strategy then return failure('release session before changing strategy') end
if not session then
    local count = 0
    for _ in pairs(state.sessions) do count = count + 1 end
    if count >= 10000 then return failure('business session limit reached') end
    session = {owner = identity.owner, strategy = identity.strategy, active = {}, seen = {}}
    state.sessions[identity.session] = session
end

local occupied = {}
for name, other in pairs(state.sessions) do
    if other.binding then occupied[other.binding.egress] = name end
    for _, lease in pairs(other.active) do occupied[lease.egress] = name end
end

local candidates = {}
local by_id = {}
local present = {}
local exits = {}
for _, proxy in ipairs(request.proxies) do
    local pool = proxy.protocol == 'http' and KEYS[3] or KEYS[4]
    local cooldown = tonumber(redis.call('HGET', KEYS[2], proxy.id) or '0')
    local verified = redis.call('HGET', KEYS[5], proxy.id)
    local metadata = verified and cjson.decode(verified) or nil
    local exists = redis.call('HEXISTS', pool, proxy.address) == 1 and metadata
        and metadata.egress == proxy.egress and metadata.stable
    if exists then present[proxy.id] = metadata end
    if exists and metadata.verified_at * 1000 + request.max_age_ms >= now and cooldown <= now then
        proxy = metadata
        by_id[proxy.id] = proxy
        local owner = occupied[proxy.egress]
        if (not owner or owner == identity.session)
            and (request.capability ~= 'tunnel' or proxy.tunnel) and not exits[proxy.egress] then
            table.insert(candidates, proxy)
            exits[proxy.egress] = true
        end
    end
end

local chosen = nil
local changed = false
if identity.strategy == 'stick' and session.binding then
    local existing = by_id[session.binding.id]
    if existing and existing.egress == session.binding.egress then
        if request.capability == 'tunnel' and not existing.tunnel then return failure('bound proxy cannot create target tunnel') end
        chosen = existing
    else
        local known = present[session.binding.id]
        if known and known.egress == session.binding.egress then
            return failure('bound proxy is cooling down or awaiting fresh verification')
        end
        session.previous = session.binding
        session.binding = nil
    end
end

if not chosen then
    if #candidates == 0 then return failure('no unoccupied verified egress available') end
    local eligible = {}
    if identity.strategy == 'random' then
        for _, proxy in ipairs(candidates) do
            if proxy.egress ~= session.last then table.insert(eligible, proxy) end
        end
        if #eligible == 0 then return failure('no different egress available for random') end
    elseif identity.strategy == 'shuffle' then
        for _, proxy in ipairs(candidates) do
            if not session.seen[proxy.egress] then table.insert(eligible, proxy) end
        end
        if #eligible == 0 then
            session.seen = {}
            for _, proxy in ipairs(candidates) do
                if #candidates == 1 or proxy.egress ~= session.last then table.insert(eligible, proxy) end
            end
        end
    else
        eligible = candidates
    end
    if identity.strategy == 'round-robin' then
        chosen = eligible[(state.cursor % #eligible) + 1]
        state.cursor = state.cursor + 1
    else
        chosen = eligible[(request.random % #eligible) + 1]
    end
    if identity.strategy == 'stick' then
        changed = session.previous ~= nil
        session.binding = chosen
        session.previous = nil
    end
end
session.last = chosen.egress
session.seen[chosen.egress] = true
session.active[request.token] = {deadline = now + request.lease_ms, egress = chosen.egress, proxy_id = chosen.id}
return save({proxy = chosen, changed = changed})
