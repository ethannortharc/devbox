-- Devbox yazi init.lua — enhanced preview with bat and glow
-- This file is pushed to ~/.config/yazi/init.lua inside the VM

-- Use bat for code preview with syntax highlighting
-- Yazi uses its own built-in previewer by default, but we can enhance
-- the status line to show useful info
Header:children_add(function()
	if ya.target_family() ~= "unix" then
		return ui.Line {}
	end
	return ui.Span(" devbox"):fg("blue")
end, 500, Header.RIGHT)
