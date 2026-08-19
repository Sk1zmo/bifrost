// Bifrost for Android and iOS.
//
// One Flutter codebase over the same Rust core the desktop runs, so a phone is
// a real peer on the network — it discovers, chats, transfers and calls
// directly, rather than remote-controlling a laptop.

import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import 'screens/chat_screen.dart';
import 'screens/devices_screen.dart';
import 'screens/files_screen.dart';
import 'screens/settings_screen.dart';
import 'state/node_state.dart';

void main() {
  WidgetsFlutterBinding.ensureInitialized();
  runApp(const BifrostApp());
}

/// The palette, matching the web dashboard so the two read as one product.
class BifrostColors {
  static const seriesOne = Color(0xFF2A78D6);
  static const seriesOneDark = Color(0xFF3987E5);
  static const seriesTwo = Color(0xFFEB6834);
  static const good = Color(0xFF0CA30C);
  static const warning = Color(0xFFFAB219);
  static const critical = Color(0xFFD03B3B);
}

class BifrostApp extends StatelessWidget {
  const BifrostApp({super.key});

  @override
  Widget build(BuildContext context) {
    return ChangeNotifierProvider(
      create: (_) => NodeState()..initialise(),
      child: MaterialApp(
        title: 'Bifrost',
        debugShowCheckedModeBanner: false,
        theme: _theme(Brightness.light),
        darkTheme: _theme(Brightness.dark),
        home: const RootScreen(),
      ),
    );
  }

  ThemeData _theme(Brightness brightness) {
    final dark = brightness == Brightness.dark;
    return ThemeData(
      brightness: brightness,
      useMaterial3: true,
      colorScheme: ColorScheme.fromSeed(
        seedColor: dark ? BifrostColors.seriesOneDark : BifrostColors.seriesOne,
        brightness: brightness,
      ),
      scaffoldBackgroundColor: dark ? const Color(0xFF0D0D0D) : const Color(0xFFF9F9F7),
      cardTheme: CardTheme(
        elevation: 0,
        color: dark ? const Color(0xFF1A1A19) : const Color(0xFFFCFCFB),
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(12),
          side: BorderSide(
            color: dark ? Colors.white.withOpacity(.10) : Colors.black.withOpacity(.10),
          ),
        ),
      ),
    );
  }
}

class RootScreen extends StatefulWidget {
  const RootScreen({super.key});

  @override
  State<RootScreen> createState() => _RootScreenState();
}

class _RootScreenState extends State<RootScreen> {
  int _tab = 0;

  static const _screens = [
    DevicesScreen(),
    ChatScreen(),
    FilesScreen(),
    SettingsScreen(),
  ];

  @override
  Widget build(BuildContext context) {
    final state = context.watch<NodeState>();

    if (state.starting) {
      return const Scaffold(
        body: Center(
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              CircularProgressIndicator(),
              SizedBox(height: 16),
              Text('Joining the network…'),
            ],
          ),
        ),
      );
    }

    if (state.startupError != null) {
      return Scaffold(
        body: Padding(
          padding: const EdgeInsets.all(24),
          child: Center(
            child: Column(
              mainAxisSize: MainAxisSize.min,
              children: [
                const Icon(Icons.wifi_off, size: 48),
                const SizedBox(height: 16),
                Text('Bifrost could not start',
                    style: Theme.of(context).textTheme.titleMedium),
                const SizedBox(height: 8),
                Text(
                  state.startupError!,
                  textAlign: TextAlign.center,
                  style: Theme.of(context).textTheme.bodySmall,
                ),
                const SizedBox(height: 20),
                FilledButton(
                  onPressed: () => context.read<NodeState>().initialise(),
                  child: const Text('Try again'),
                ),
              ],
            ),
          ),
        ),
      );
    }

    return Scaffold(
      body: SafeArea(child: _screens[_tab]),
      bottomNavigationBar: NavigationBar(
        selectedIndex: _tab,
        onDestinationSelected: (index) => setState(() => _tab = index),
        destinations: [
          NavigationDestination(
            icon: Badge(
              isLabelVisible: state.onlinePeers.isNotEmpty,
              label: Text('${state.onlinePeers.length}'),
              child: const Icon(Icons.devices_outlined),
            ),
            selectedIcon: const Icon(Icons.devices),
            label: 'Devices',
          ),
          const NavigationDestination(
            icon: Icon(Icons.chat_bubble_outline),
            selectedIcon: Icon(Icons.chat_bubble),
            label: 'Chat',
          ),
          const NavigationDestination(
            icon: Icon(Icons.folder_outlined),
            selectedIcon: Icon(Icons.folder),
            label: 'Files',
          ),
          const NavigationDestination(
            icon: Icon(Icons.settings_outlined),
            selectedIcon: Icon(Icons.settings),
            label: 'Settings',
          ),
        ],
      ),
    );
  }
}

// ─── Shared widgets ──────────────────────────────────────────────────────────

/// A labelled bar with the value printed beside it.
///
/// The number is always shown, so the colour is a secondary cue rather than
/// the only one — the same rule the web dashboard follows.
class Meter extends StatelessWidget {
  const Meter({
    super.key,
    required this.label,
    required this.fraction,
    required this.value,
  });

  final String label;
  final double fraction;
  final String value;

  @override
  Widget build(BuildContext context) {
    final pct = fraction.clamp(0.0, 1.0);
    final colour = pct >= .9
        ? BifrostColors.critical
        : pct >= .75
            ? BifrostColors.warning
            : Theme.of(context).colorScheme.primary;

    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 5),
      child: Row(
        children: [
          SizedBox(
            width: 44,
            child: Text(label, style: Theme.of(context).textTheme.bodySmall),
          ),
          Expanded(
            child: ClipRRect(
              borderRadius: BorderRadius.circular(5),
              child: LinearProgressIndicator(
                value: pct,
                minHeight: 8,
                backgroundColor: colour.withOpacity(.18),
                valueColor: AlwaysStoppedAnimation(colour),
              ),
            ),
          ),
          const SizedBox(width: 10),
          SizedBox(
            width: 96,
            child: Text(
              value,
              textAlign: TextAlign.right,
              style: Theme.of(context).textTheme.bodySmall,
            ),
          ),
        ],
      ),
    );
  }
}

/// The icon for a device type, matching the web dashboard's mapping.
IconData deviceIcon(String deviceType) {
  final d = deviceType.toLowerCase();
  if (d.contains('phone') || d.contains('android') || d.contains('ios')) {
    return Icons.smartphone;
  }
  if (d.contains('tablet') || d.contains('ipad')) return Icons.tablet_mac;
  if (d.contains('server') || d.contains('pi')) return Icons.dns_outlined;
  if (d.contains('mac')) return Icons.laptop_mac;
  if (d.contains('windows')) return Icons.desktop_windows_outlined;
  if (d.contains('linux')) return Icons.laptop_outlined;
  return Icons.devices_other;
}

String formatBytes(num bytes) {
  if (bytes <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  var value = bytes.toDouble();
  var unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  return '${value >= 100 || unit == 0 ? value.round() : value.toStringAsFixed(1)} ${units[unit]}';
}

String formatMbAsGb(int mb) => '${(mb / 1024).toStringAsFixed(1)} GB';
