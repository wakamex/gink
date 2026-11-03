// gink: a C app on gesso over a Rust core (core/, built by Cargo into a static library).
//   zig build run                          build and run
//   zig build --fork=../gesso              build against a local gesso checkout
//   zig build -Doptimize=ReleaseSmall -Dtarget=x86_64-windows-gnu -p zig-out/windows
//   zig build -Dtarget=x86_64-linux-android -Dandroid_ndk=PATH   libmain.so for Android (tools/android-apk packs it)
// Needs cargo and protoc on PATH (or PROTOC set). tools/build-core runs Cargo, with the C inside the
// core's dependencies compiled by `zig cc` for the app's target, so one host builds Linux and Windows.
const std = @import("std");
const gesso_build = @import("gesso");

const flags: []const []const u8 = &.{ "-std=c11", "-ffp-contract=off", "-Wall", "-Wextra", "-Wno-unused-parameter", "-Wno-missing-field-initializers" };

pub fn build(b: *std.Build) void {
    const target = b.standardTargetOptions(.{});
    const optimize = b.standardOptimizeOption(.{});
    const t = target.result;
    // Android takes the NDK's C library and system libraries, as gesso and SDL do.
    const android = t.abi.isAndroid();
    const android_ndk = b.option([]const u8, "android_ndk", "Android NDK root, for building for Android");
    if (android and android_ndk == null) @panic("-Dandroid_ndk=PATH is needed to build for Android");
    // (gesso plays AAC, which is what phones record voice notes in, only with its video modules)
    const gesso_dep = b.dependency("gesso", .{ .target = target, .optimize = optimize, .video = true, .android_ndk = android_ndk });
    const gesso = gesso_dep.artifact("gesso");
    // (-Dstrip=false keeps the symbols of a release build, for reading a crash: on Windows as a .pdb beside the program)
    const strip = b.option(bool, "strip", "Leave out debugging symbols (default: in every build but a debug one)") orelse (optimize != .debug);

    // The core, for Rust's name of the same target.
    const rust_target = b.fmt("{s}-{s}", .{ @tagName(t.cpu.arch), switch (t.os.tag) {
        .linux => if (android) "linux-android" else "unknown-linux-gnu",
        .windows => "pc-windows-gnu",
        .macos => "apple-darwin",
        else => @panic("gink builds for Linux, Windows and macOS"),
    } });
    const profile = if (optimize == .debug) "debug" else "release";
    const zig_target = target.query.zigTriple(b.allocator) catch @panic("OOM");
    const cargo = b.addSystemCommand(&.{ "tools/build-core", rust_target, zig_target, profile });
    cargo.setEnvironmentVariable("GINK_ZIG", b.graph.zig_exe);
    if (android) {
        cargo.setEnvironmentVariable("GINK_ANDROID_NDK", android_ndk.?);
        cargo.setEnvironmentVariable("GINK_ANDROID_API", b.fmt("{d}", .{t.os.version_range.linux.android}));
    }
    cargo.has_side_effects = true;  // Cargo decides what is stale

    // On Android the app is libmain.so, which SDL's activity loads and runs main() from; tools/android-apk
    // packs it with the fonts into an APK.
    if (android) {
        const mod = b.createModule(.{ .target = target, .optimize = optimize, .link_libc = true, .strip = strip, .pic = true });
        mod.addCSourceFiles(.{ .root = b.path("src"), .files = &.{ "main.c", "key.c" }, .flags = flags });
        mod.addIncludePath(b.path("include"));
        mod.linkLibrary(gesso);
        mod.addObjectFile(b.path(b.fmt("core/target/{s}/{s}/{s}/libgsignal_core.a", .{ zig_target, rust_target, profile })));
        for ([_][]const u8{ "log", "dl", "m" }) |lib| mod.linkSystemLibrary(lib, .{});
        const lib = b.addLibrary(.{ .linkage = .dynamic, .name = "main", .root_module = mod });
        // (the same SDL that gesso's build made, for the NDK's C library it found)
        // (the NDK's C library and system libraries, as gesso's build found them)
        gesso_build.useNdk(lib, gesso_dep);
        lib.step.dependOn(&cargo.step);
        b.installArtifact(lib);
        // (and the Java of SDL and gesso, which the APK compiles with gink's activity)
        b.installDirectory(.{ .source_dir = gesso_dep.namedLazyPath("android_java"), .install_dir = .prefix, .install_subdir = "java" });
        return;
    }
    // Two programs over the same core: the app, and gink-core, which drives the core from a terminal.
    var exes: [2]*std.Build.Step.Compile = undefined;
    for ([_][]const u8{ "gink", "gink-core" }, [_][]const []const u8{ &.{ "main.c", "key.c" }, &.{ "core_cli.c", "key.c" } }, &exes) |name, files, *out| {
        const mod = b.createModule(.{
            .target = target,
            .optimize = optimize,
            .link_libc = true,
            .strip = strip,
        });
        mod.addCSourceFiles(.{ .root = b.path("src"), .files = files, .flags = flags });
        mod.addIncludePath(b.path("include"));
        mod.linkLibrary(gesso);
        mod.addObjectFile(b.path(b.fmt("core/target/{s}/{s}/{s}/libgsignal_core.a", .{ zig_target, rust_target, profile })));
        mod.linkSystemLibrary("unwind", .{});  // Rust's standard library walks the stack with it
        if (t.os.tag == .windows) for ([_][]const u8{ "ole32", "advapi32", "crypt32", "ws2_32", "userenv", "bcrypt", "ntdll" }) |lib| mod.linkSystemLibrary(lib, .{});
        // (the core checks the service's certificates with the system's verifier on macOS)
        if (t.os.tag == .macos) for ([_][]const u8{ "Security", "CoreFoundation" }) |framework| mod.linkFramework(framework, .{});
        const exe = b.addExecutable(.{ .name = name, .root_module = mod });
        exe.step.dependOn(&cargo.step);
        b.installArtifact(exe);
        out.* = exe;
    }
    // (the fonts the app looks for beside itself; see assets/NOTICE.md)
    b.installBinFile("assets/ClankerTwemoji.ttf", "ClankerTwemoji.ttf");
    b.installBinFile("assets/ClankerMono-Regular.ttf", "ClankerMono-Regular.ttf");
    b.installBinFile("assets/Inter-Regular.ttf", "Inter-Regular.ttf");
    // (and the licences that go with the program wherever it is copied: tools/third-party-licenses
    // adds the compiled-in libraries' when a build is published)
    b.installBinFile("LICENSE", "LICENSE");
    b.installBinFile("assets/NOTICE.md", "NOTICE.md");
    b.installBinFile("assets/ClankerMono-LICENSE.txt", "ClankerMono-LICENSE.txt");
    b.installBinFile("assets/Inter-LICENSE.txt", "Inter-LICENSE.txt");
    const exe = exes[0];
    if (t.os.tag == .windows) exe.subsystem = .windows;
    // (the icon Windows shows for the program and its window; assets/icon/update.sh makes it)
    if (t.os.tag == .windows) exe.root_module.addWin32ResourceFile(.{ .file = b.path("assets/icon/gink.rc") });
    const run = b.addRunArtifact(exe);
    run.step.dependOn(b.getInstallStep());
    run.addPassthruArgs();
    b.step("run", "Build and run gink").dependOn(&run.step);
}
