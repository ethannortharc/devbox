-- Glow markdown previewer for yazi
-- Renders markdown files with glow in the preview pane

local M = {}

function M:peek(job)
	local child = Command("glow")
		:args({ "-s", "dark", "-w", tostring(job.area.w), tostring(job.file.url) })
		:stdout(Command.PIPED)
		:stderr(Command.PIPED)
		:spawn()

	if not child then
		-- Fallback to plain text if glow is not available
		local limit = job.area.h
		local i, lines = 0, ""
		local file = io.open(tostring(job.file.url), "r")
		if file then
			for line in file:lines() do
				i = i + 1
				if i > job.skip + limit then break end
				if i > job.skip then
					lines = lines .. line .. "\n"
				end
			end
			file:close()
		end
		ya.preview_widgets(job, { ui.Text.parse(lines):area(job.area) })
		return
	end

	local output = child:wait_with_output()
	if output and output.status and output.status.success then
		ya.preview_widgets(job, { ui.Text.parse(output.stdout):area(job.area) })
	else
		-- Fallback on failure
		ya.preview_widgets(job, { ui.Text("Failed to render markdown"):area(job.area) })
	end
end

function M:seek(job)
	-- No seek support for rendered markdown
end

return M
