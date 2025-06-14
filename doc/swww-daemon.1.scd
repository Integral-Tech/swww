swww-daemon(1)

# NAME
swww-daemon

# SYNOPSIS
swww-daemon [OPTIONS]

# OPTIONS

*-f*,*--format* <xrgb|xbgr|rgb|bgr>
	Force the daemon to use a specific wl_shm format.

	IMPORTANT: make sure this is a value your compositor actually supports!
	'swww-daemon' will automatically select the best format for itself during
	initialization; this is only here for fallback, debug, and workaround
	purposes.

*-l*,*--layer* <background|bottom>
	Which layer to display the background in. Defaults to `background`.

	We do not accept layers `top` and `overlay` because those would make your
	desktoop unusable by simply putting an image on top of everything else. If
	there is ever a use case for these, we can reconsider it.

*-n*,*--namespace* <namespace>
	Which wayland namespace to append to `swww-daemon`.

	The result namespace will the `swww-daemon<specified namespace>`. This also
	affects the name of the `swww-daemon` socket we will use to communicate
	with the `client`. Specifically, our socket name is
	`${WAYLAND_DISPLAY}-swww-daemon.<specified namespace>.socket`.

	Some compositors can have several different wallpapers per output. This
	allows you to differentiate between them. Most users will probably not have
	to set anything in this option.

*--no-cache*
	Don't search the cache for the last wallpaper for each output.
	Useful if you always want to select which image 'swww' loads manually using
	'swww img'

*-q*,*--quiet*
	Makes the daemon only log errors.

*-h*, *--help*
	Print help (see a summary with '-h')

*-V*, *--version*
	Print version

# DESCRIPTION

The *swww-daemon* will run continuously, waiting for commands in
_${XDG_RUNTIME_DIR}/swww-${WAYLAND_DISPLAY}.socket_ (or
_/tmp/swww/swww-${WAYLAND_DISPLAY}.socket_, if $XDG_RUNTIME_DIR is not set). The
daemon will take care of both creating and deleting that file when it is
initialized or killed.

# SEE ALSO
*swww*(1)
