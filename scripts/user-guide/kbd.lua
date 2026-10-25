-- Pandoc filter for the user guide PDF (scripts/user-guide.sh): code spans that are keys,
-- such as `⌘K`, `⇧S`, `P`, `Space` or `Esc`, become <kbd> so guide.css draws them as keys.
-- Other code spans (paths, tokens such as `{date}`) stay code.

local named = { Space = true, Tab = true, Esc = true }
local modifiers = { "⌘", "⌥", "⇧", "⌃", "←", "→", "↑", "↓", "↩" }

local function is_key(s)
  if named[s] or utf8.len(s) == 1 then
    return true
  end
  if s:find("%s") then
    return false
  end
  for _, m in ipairs(modifiers) do
    if s:find(m, 1, true) then
      return true
    end
  end
  return false
end

function Code(el)
  if is_key(el.text) then
    local text = el.text:gsub("&", "&amp;"):gsub("<", "&lt;"):gsub(">", "&gt;")
    return pandoc.RawInline("html", "<kbd>" .. text .. "</kbd>")
  end
end
