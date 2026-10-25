-- agent: the in-page assistant.
--
-- A conversation with an OpenAI-compatible model (Ollama, LM Studio, vLLM,
-- OpenAI) through the cloud module's `cloud.chat` stream. Its tools are the
-- page's own actions (page.actions()), the same semantic port external agents
-- use, so whatever a person can do from the toolbar an agent can do too.
--
-- The loop: the user's message goes out with the tools; when a reply ends
-- with tool calls, each one runs with page.act(name, args), its result goes
-- back as a `tool` message, and the model is asked again, until it answers
-- without tools (or MAX_ROUNDS is reached). Every step is kept in the
-- `assistant.steps` cell, which the chat panel shows and agents can read.
--
-- A page script reacts to data only in handlers; there is no "stream item
-- arrived" handler, so the page calls `Agent:poll()` from the done callback
-- of a job that takes `assistant.signal` (the finished reply) as an input
-- (see main.luau and NOTES.md).

local U = import("pdfutil")

local Agent = {}
Agent.__index = Agent

local MAX_ROUNDS = 10
local MAX_RESULT = 6000 -- characters of one tool result sent back to the model
local MAX_STEPS = 200
local MAX_CONVO = 60000 -- characters of conversation kept for the model

Agent.PRESETS = {
	{ label = "Ollama 本機 · localhost:11434", base = "http://localhost:11434/v1", model = "llama3.2" },
	{ label = "LM Studio · localhost:1234", base = "http://localhost:1234/v1", model = "local-model" },
	{ label = "OpenAI · api.openai.com", base = "https://api.openai.com/v1", model = "gpt-4o-mini" },
}

-- The OpenAI key goes to OpenAI only: compare scheme and host, never a
-- substring (the endpoint is agent-writable).
function Agent.needs_key(url)
	local u = string.lower(tostring(url or ""))
	return u == "https://api.openai.com" or string.match(u, "^https://api%.openai%.com[/?]") ~= nil
end

-- Tool names the OpenAI API accepts.
local function tool_name_ok(name)
	return type(name) == "string" and #name <= 64 and string.match(name, "^[%a_][%w_%-]*$") ~= nil
end

-- JSON-schema parameters from an action's params: a schema is used as is; a
-- table of name -> description (the form the UI library uses) becomes
-- string properties.
local function schema_of(params)
	if type(params) ~= "table" then
		return { type = "object" }
	end
	if params.type == "object" then
		return params
	end
	local props = {}
	local any = false
	for k, v in params do
		if type(k) == "string" then
			props[k] = { type = "string", description = tostring(v) }
			any = true
		end
	end
	if not any then
		return { type = "object" }
	end
	return { type = "object", properties = props }
end

-- Tools for the model: every action whose name the API accepts (the UI
-- library's "<app>.describe" style actions and the assistant's own
-- "assistant.*" actions have dots, so they are left out), minus the names in
-- `exclude` (actions that would discard the person's work: the model reads
-- PDF text, so a crafted document could ask for them).
function Agent.tools_from(actions, exclude)
	exclude = exclude or {}
	local tools, names = {}, {}
	for _, a in actions or {} do
		if tool_name_ok(a.name) and not exclude[a.name] then
			table.insert(tools, {
				type = "function",
				["function"] = { name = a.name, description = U.clip(a.description or "", 900), parameters = schema_of(a.params) },
			})
			names[a.name] = true
		end
	end
	return tools, names
end

-- A string from an agent-writable cell (anything else reads as "").
local function text_of(v)
	if type(v) == "string" then
		return v
	elseif type(v) == "number" then
		return tostring(v)
	end
	return ""
end
Agent.text_of = text_of

-- opts: available (cloud module present), system() -> prompt text,
-- on_tool(name, args, ok, result) after each tool ran, exclude = { name = true }
-- (actions never offered to the model).
function Agent.new(opts)
	local self = setmetatable({}, Agent)
	self.available = opts.available == true
	self.system = opts.system
	self.on_tool = opts.on_tool
	self.exclude = opts.exclude or {}
	self.base_url = page.input("assistant.base_url", Agent.PRESETS[1].base, { agent = true })
	self.model = page.input("assistant.model", Agent.PRESETS[1].model, { agent = true })
	self.steps = page.input("assistant.steps", {})
	self.busy = page.input("assistant.busy", false)
	self.request = page.input("assistant.request", nil)
	if self.available then
		self.chat = page.stream("assistant.chat", "cloud.chat", { request = self.request, latest = true })
		-- the finished reply (nil while one streams): changes once per reply
		self.signal = page.derive("assistant.signal", { self.chat }, function(c)
			local last = if type(c) == "table" then c.last else nil
			if type(last) == "table" and last.done then
				return last
			end
			return nil
		end)
	else
		self.chat = page.input("assistant.chat", nil)
		self.signal = page.derive("assistant.signal", {}, function()
			return nil
		end)
	end
	self.convo = {}
	self.rounds = 0
	self.turn = 0
	self.processed = -1
	self.tools = nil
	self.tool_names = nil
	self.in_tool = false
	return self
end

function Agent:step(s)
	local list = page.get(self.steps)
	if type(list) ~= "table" then
		list = {}
	end
	list = table.clone(list)
	table.insert(list, s)
	while #list > MAX_STEPS do
		table.remove(list, 1)
	end
	page.set(self.steps, list)
	if self.on_step then
		self.on_step()
	end
end

function Agent:is_busy()
	return page.get(self.busy) == true
end

-- The endpoint (agent-writable: a non-string reads as "").
function Agent:url()
	return U.trim(text_of(page.get(self.base_url)))
end

-- Drop the oldest exchanges (a user message and everything after it up to
-- the next user message) until the conversation fits.
function Agent:_trim()
	local function size()
		local n = 0
		for _, m in self.convo do
			n += #(m.content or "") + 200
		end
		return n
	end
	while size() > MAX_CONVO and #self.convo > 1 do
		table.remove(self.convo, 1)
		while #self.convo > 1 and self.convo[1].role ~= "user" do
			table.remove(self.convo, 1)
		end
	end
end

function Agent:_send()
	if self.tools == nil then
		self.tools, self.tool_names = Agent.tools_from(page.actions(), self.exclude)
	end
	self.rounds += 1
	self.turn += 1
	local url = self:url()
	local messages = { { role = "system", content = self.system() } }
	for _, m in self.convo do
		table.insert(messages, m)
	end
	page.set(self.request, {
		base_url = url,
		model = text_of(page.get(self.model)),
		messages = messages,
		tools = self.tools,
		tool_choice = "auto",
		temperature = 0.2,
		max_tokens = 1200,
		timeout = 180,
		secret = if Agent.needs_key(url) then "openai" else nil,
		turn = self.turn,
	})
	page.set(self.busy, true)
end

function Agent:_finish()
	page.set(self.busy, false)
end

-- Ask the model. Returns { ok, error }.
function Agent:ask(textv)
	local t = U.trim(textv)
	if t == "" then
		return { ok = false, error = "empty message" }
	end
	if not self.available then
		self:step({ kind = "error", text = "這個平台沒有 cloud 模組，助理無法連線。The cloud module is not available here." })
		return { ok = false, error = "the cloud module is not available on this platform" }
	end
	if self:is_busy() then
		return { ok = false, error = "the assistant is still working on the previous message" }
	end
	local url = self:url()
	local check = cloud.check_url(url .. "/chat/completions")
	if not check.ok then
		self:step({ kind = "user", text = t })
		self:step({ kind = "error", text = "端點不允許 endpoint not allowed: " .. tostring(check.error) })
		return { ok = false, error = check.error }
	end
	self.rounds = 0
	table.insert(self.convo, { role = "user", content = t })
	self:_trim()
	self:step({ kind = "user", text = t })
	self:_send()
	return { ok = true, turn = self.turn }
end

function Agent:stop()
	if self:is_busy() then
		page.set(self.request, nil)
		self:_finish()
		self:step({ kind = "info", text = "已停止 Stopped" })
		return true
	end
	return false
end

function Agent:clear()
	page.set(self.request, nil)
	self:_finish()
	self.convo = {}
	page.set(self.steps, {})
end

function Agent:configure(base, model)
	if base ~= nil then
		page.set(self.base_url, U.trim(text_of(base)))
	end
	if model ~= nil then
		page.set(self.model, U.trim(text_of(model)))
	end
end

local function encode(v)
	local ok, s = pcall(json.encode, v)
	if ok then
		return s
	end
	return json.encode({ error = "result could not be encoded: " .. tostring(s) })
end

-- A short description of a tool result for the step list.
local function brief(res)
	if type(res) ~= "table" then
		return U.clip(tostring(res), 160)
	end
	if res.error then
		return "錯誤 error: " .. U.clip(tostring(res.error), 160)
	end
	if res.summary then
		return U.clip(tostring(res.summary), 200)
	end
	return U.clip(encode(res), 200)
end

function Agent:_run_tool(call)
	local name = tostring(call.name or "")
	local args = call.args
	if args == nil then
		local s = call.arguments or ""
		if U.trim(s) == "" then
			args = {}
		else
			local ok, v = pcall(json.decode, s)
			if ok then
				args = v
			else
				local err = { error = "arguments are not valid JSON: " .. U.clip(s, 200) }
				return false, err, "{}"
			end
		end
	end
	if type(args) ~= "table" then
		args = {}
	end
	local args_text = U.clip(encode(args), 300)
	if not self.tool_names[name] then
		return false, { error = "no tool named \"" .. name .. "\"" }, args_text
	end
	self.in_tool = true
	local ok, res = pcall(page.act, name, args)
	self.in_tool = false
	if not ok then
		return false, { error = U.clip(tostring(res), 600) }, args_text
	end
	if res == nil then
		res = { ok = true }
	end
	return true, res, args_text
end

function Agent:_handle(item)
	local err = item.error
	if type(err) == "string" and err ~= "" then
		self:step({ kind = "error", text = U.clip(err, 600) })
		self:_finish()
		return
	end
	if item.cancelled then
		self:_finish()
		return
	end
	local calls = item.tool_calls or {}
	local said = item.text or ""
	if #calls == 0 then
		if U.trim(said) == "" then
			said = "（模型沒有回覆文字 the model sent no text）"
		end
		table.insert(self.convo, { role = "assistant", content = said })
		self:step({ kind = "assistant", text = said })
		self:_finish()
		return
	end
	local msg_calls = {}
	for i, c in calls do
		local id = if type(c.id) == "string" and c.id ~= "" then c.id else ("call_" .. self.turn .. "_" .. i)
		msg_calls[i] = { id = id, type = "function", ["function"] = { name = c.name, arguments = if (c.arguments or "") ~= "" then c.arguments else "{}" } }
	end
	table.insert(self.convo, { role = "assistant", content = said, tool_calls = msg_calls })
	if U.trim(said) ~= "" then
		self:step({ kind = "assistant", text = said, partial = true })
	end
	for i, c in calls do
		local t0 = os.clock()
		local ok, res, args_text = self:_run_tool(c)
		local ms = (os.clock() - t0) * 1000
		local body = encode(res)
		if #body > MAX_RESULT then
			body = string.sub(body, 1, MAX_RESULT) .. "…(cut)"
		end
		table.insert(self.convo, { role = "tool", tool_call_id = msg_calls[i].id, content = body })
		self:step({ kind = "tool", name = tostring(c.name), args = args_text, result = brief(res), ok = ok, ms = math.floor(ms * 10) / 10 })
		if self.on_tool then
			self.on_tool(c.name, ok, res)
		end
	end
	if self.rounds >= MAX_ROUNDS then
		self:step({ kind = "error", text = string.format("已達 %d 輪工具呼叫上限，停止。Stopped after %d rounds of tool calls.", MAX_ROUNDS, MAX_ROUNDS) })
		self:_finish()
		return
	end
	self:_send()
end

-- Look at the finished reply, once. Call from a handler (see the header).
function Agent:poll()
	if not self.available then
		return
	end
	local item = page.peek(self.signal)
	if type(item) ~= "table" or item.done ~= true or item.state ~= nil then
		return
	end
	local seq = tonumber(item.seq) or 0
	if seq <= self.processed then
		return
	end
	self.processed = seq
	if not self:is_busy() then
		return -- a reply nobody waits for (stopped, cleared)
	end
	self:_handle(item)
end

return Agent
