// Settings, active calls, and the activity log.

import 'package:flutter/material.dart';
import 'package:provider/provider.dart';
import 'package:url_launcher/url_launcher.dart';

import '../main.dart';
import '../state/node_state.dart';

class SettingsScreen extends StatefulWidget {
  const SettingsScreen({super.key});

  @override
  State<SettingsScreen> createState() => _SettingsScreenState();
}

class _SettingsScreenState extends State<SettingsScreen> {
  final _name = TextEditingController();
  bool _nameLoaded = false;

  @override
  void dispose() {
    _name.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final state = context.watch<NodeState>();
    final snapshot = state.snapshot;
    if (snapshot == null) return const Center(child: CircularProgressIndicator());

    // Seed the field once, so it is not overwritten while being edited.
    if (!_nameLoaded) {
      _name.text = snapshot.username;
      _nameLoaded = true;
    }

    final calls = state.calls.where((c) => c.isLive).toList();

    return ListView(
      padding: const EdgeInsets.fromLTRB(14, 14, 14, 28),
      children: [
        if (calls.isNotEmpty) ...[
          Card(
            child: Padding(
              padding: const EdgeInsets.all(16),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text('Active calls',
                      style: Theme.of(context).textTheme.titleSmall),
                  const SizedBox(height: 8),
                  ...calls.map(
                    (call) => ListTile(
                      contentPadding: EdgeInsets.zero,
                      leading: const Icon(Icons.call),
                      title: Text(call.peerName),
                      subtitle: Text(
                        '${call.state} · ${call.framesSent} sent / '
                        '${call.framesReceived} received',
                      ),
                      trailing: TextButton(
                        onPressed: () => state.hangUp(session: call.sessionId),
                        child: const Text('End'),
                      ),
                    ),
                  ),
                ],
              ),
            ),
          ),
          const SizedBox(height: 14),
        ],
        Card(
          child: Padding(
            padding: const EdgeInsets.all(16),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text('This device', style: Theme.of(context).textTheme.titleSmall),
                const SizedBox(height: 12),
                Row(
                  children: [
                    Expanded(
                      child: TextField(
                        controller: _name,
                        decoration: const InputDecoration(
                          labelText: 'Display name',
                          border: OutlineInputBorder(),
                          isDense: true,
                        ),
                      ),
                    ),
                    const SizedBox(width: 8),
                    FilledButton(
                      onPressed: () {
                        final name = _name.text.trim();
                        if (name.isNotEmpty) state.rename(name);
                      },
                      child: const Text('Save'),
                    ),
                  ],
                ),
                const SizedBox(height: 8),
                SwitchListTile(
                  contentPadding: EdgeInsets.zero,
                  title: const Text('Share resources'),
                  subtitle: Text(
                    snapshot.shared.enabled
                        ? 'CPU ${snapshot.shared.cpuOfferedPct.round()}% · '
                            'RAM ${formatMbAsGb(snapshot.shared.ramOfferedMb)}'
                        : 'Nothing advertised',
                  ),
                  value: snapshot.sharingEnabled,
                  onChanged: state.setSharing,
                ),
              ],
            ),
          ),
        ),
        const SizedBox(height: 14),
        Card(
          child: Padding(
            padding: const EdgeInsets.all(16),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text('Connection', style: Theme.of(context).textTheme.titleSmall),
                const SizedBox(height: 10),
                _Row(label: 'Address', value: snapshot.ip),
                _Row(label: 'Node id', value: snapshot.nodeId),
                _Row(label: 'Downloads', value: snapshot.downloadDir),
                _Row(label: 'Version', value: 'Bifrost ${snapshot.version}'),
                const SizedBox(height: 10),
                Text(
                  'Any device on this Wi-Fi can open a node’s dashboard in a '
                  'browser — no app needed there.',
                  style: Theme.of(context).textTheme.bodySmall,
                ),
                const SizedBox(height: 8),
                Wrap(
                  spacing: 8,
                  children: state.onlinePeers
                      .map((peer) => OutlinedButton.icon(
                            onPressed: () => launchUrl(
                              Uri.parse(peer.dashboardUrl),
                              mode: LaunchMode.externalApplication,
                            ),
                            icon: const Icon(Icons.open_in_browser, size: 17),
                            label: Text(peer.displayName),
                          ))
                      .toList(),
                ),
              ],
            ),
          ),
        ),
        const SizedBox(height: 14),
        Card(
          child: Padding(
            padding: const EdgeInsets.all(16),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text('Activity', style: Theme.of(context).textTheme.titleSmall),
                const SizedBox(height: 8),
                if (state.log.isEmpty)
                  const Text('Nothing yet.')
                else
                  ...state.log.take(40).map(
                        (line) => Padding(
                          padding: const EdgeInsets.symmetric(vertical: 2),
                          child: Row(
                            crossAxisAlignment: CrossAxisAlignment.start,
                            children: [
                              Text(
                                '${line.at.hour.toString().padLeft(2, "0")}:'
                                '${line.at.minute.toString().padLeft(2, "0")}',
                                style: Theme.of(context).textTheme.bodySmall,
                              ),
                              const SizedBox(width: 8),
                              Expanded(
                                child: Text(
                                  line.text,
                                  style: TextStyle(
                                    fontSize: 13,
                                    color: switch (line.level) {
                                      'success' => BifrostColors.good,
                                      'error' => BifrostColors.critical,
                                      _ => null,
                                    },
                                  ),
                                ),
                              ),
                            ],
                          ),
                        ),
                      ),
              ],
            ),
          ),
        ),
      ],
    );
  }
}

class _Row extends StatelessWidget {
  const _Row({required this.label, required this.value});
  final String label;
  final String value;

  @override
  Widget build(BuildContext context) => Padding(
        padding: const EdgeInsets.symmetric(vertical: 4),
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            SizedBox(
              width: 92,
              child: Text(label, style: Theme.of(context).textTheme.bodySmall),
            ),
            Expanded(child: SelectableText(value, style: const TextStyle(fontSize: 13))),
          ],
        ),
      );
}
