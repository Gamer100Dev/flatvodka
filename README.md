# FlatVodka

FlatVodka is a tool for managing and running Flatpak applications inside FreeBSD jails with seamless integration. It allows you to install, run, and manage Flatpak apps in a containerized environment, leveraging FreeBSD's jail system to isolate applications while maintaining easy access to host resources.

---

## Features

- Install Flatpak applications from Flathub or local `.flatpakref` files
- Run applications inside a FreeBSD jail with proper filesystem and resource mounting
- Handle Vulkan, OpenGL, and other graphics libraries
- Mount host resources like X11, Wayland, PulseAudio, and fonts
- Inject necessary libraries into the jail for compatibility
- Manage application lifecycle with cleanup and listing commands

---



## Notes

- The script relies on FreeBSD-specific features like jails.
- Ensure `ostree` is correctly installed in `/compat/ubuntu/usr/bin/ostree`.
- Adjust paths and configurations according to your environment.
- For debugging, the jail filesystem remains mounted after execution.

---

## License

This project is licensed under the BSD License. See [LICENSE](LICENSE) for details.

---

## Contributing

Contributions are welcome! Please open issues or pull requests for improvements.

---

Feel free to replace placeholders like `https://github.com/yourusername/flatvodka.git` with your actual repository URL, and add any additional details you want to include!
