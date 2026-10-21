# libcontainer 0.7.0, patched for OG-Host

Unmodified upstream source from crates.io (youki, Apache-2.0, https://github.com/youki-dev/youki),
except `src/container/tenant_builder.rs` and `src/process/container_main_process.rs` (search
for "OG-Host patch"):

1. Tenant (exec) processes join the container's **user** namespace. Upstream leaves `user` out
   of `NAMESPACE_TYPES`, so a process exec'd into a user-namespaced container keeps the host's
   user namespace and runs as real root (inside the container's other namespaces).
2. Tenant processes keep the container's **seccomp** filter. Upstream rebuilds `linux` without
   it, so exec'd processes run unfiltered — apparently because the main process then demands
   container state the tenant path doesn't have. That state is only used for seccomp *notify*
   listeners, so the main process now only asks for it when the profile uses one.

Both should be reported upstream; drop this copy once a fixed release exists.
