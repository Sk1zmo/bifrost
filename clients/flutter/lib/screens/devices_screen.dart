// The devices tab: this node's own resources, the network pool, and every peer.

import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../main.dart';
import '../models/models.dart';
import '../state/node_state.dart';

class DevicesScreen extends StatelessWidget {
  const DevicesScreen({super.key});

  @override
  Widget build(BuildContext context) {
    final state = context.watch<NodeState>();
    final snapshot = state.snapshot;
    if (snapshot == null) return const Center(child: CircularProgressIndicator());

    return ListView(
      padding: const EdgeInsets.fromLTRB(14, 14, 14, 28),
      children: [
        _Hero(snapshot: snapshot),
        const SizedBox(height: 14),
        _ThisDevice(snapshot: snapshot),
        const SizedBox(height: 14),
        Padding(
          padding: const EdgeInsets.only(left: 4, bottom: 8),
          child: Text('Devices on this network',
              style: Theme.of(context).textTheme.titleSmall),
        ),
        if (state.peers.isEmpty)
          const Card(
            child: Padding(
              padding: EdgeInsets.all(22),
              child: Text(
                'No other devices yet.\n\nStart Bifrost on a laptop on this same '
                'Wi-Fi — it will appear here within a few seconds.',
                textAlign: TextAlign.center,
              ),
            ),
          )
        else
          ...state.peers.map((peer) => _PeerCard(peer: peer)),
      ],
    );
  }
}

class _Hero extends StatelessWidget {
  const _Hero({required this.snapshot});
  final NodeSnapshot snapshot;

  @override
  Widget build(BuildContext context) {
    final pool = snapshot.pool;
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(18),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              crossAxisAlignment: CrossAxisAlignment.end,
              children: [
                Text('${pool.nodes}',
                    style: const TextStyle(
                        fontSize: 46, fontWeight: FontWeight.w600, height: 1)),
                const SizedBox(width: 10),
                Padding(
                  padding: const EdgeInsets.only(bottom: 6),
                  child: Text(
                    pool.nodes == 1
                        ? 'device (just this one)'
                        : 'devices on this network',
                    style: Theme.of(context).textTheme.bodyMedium,
                  ),
                ),
              ],
            ),
            const SizedBox(height: 14),
            Wrap(
              spacing: 22,
              runSpacing: 10,
              children: [
                _Stat(label: 'CPU cores', value: '${pool.cpuCores}'),
                _Stat(label: 'Memory free', value: formatMbAsGb(pool.ramAvailableMb)),
                _Stat(label: 'Disk free', value: '${pool.diskAvailableGb.round()} GB'),
                _Stat(label: 'Sharing', value: '${pool.sharingNodes}'),
              ],
            ),
          ],
        ),
      ),
    );
  }
}

class _Stat extends StatelessWidget {
  const _Stat({required this.label, required this.value});
  final String label;
  final String value;

  @override
  Widget build(BuildContext context) => Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(label, style: Theme.of(context).textTheme.bodySmall),
          Text(value,
              style: const TextStyle(fontSize: 19, fontWeight: FontWeight.w600)),
        ],
      );
}

class _ThisDevice extends StatelessWidget {
  const _ThisDevice({required this.snapshot});
  final NodeSnapshot snapshot;

  @override
  Widget build(BuildContext context) {
    final t = snapshot.telemetry;
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(16),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                Icon(deviceIcon(snapshot.deviceType), size: 20),
                const SizedBox(width: 9),
                Expanded(
                  child: Text(snapshot.username,
                      style: const TextStyle(fontWeight: FontWeight.w600)),
                ),
                Text(snapshot.ip, style: Theme.of(context).textTheme.bodySmall),
              ],
            ),
            const SizedBox(height: 12),
            Meter(
              label: 'CPU',
              fraction: t.cpuUsage / 100,
              value: '${t.cpuUsage.toStringAsFixed(0)}% · ${t.cpuCores} cores',
            ),
            Meter(
              label: 'RAM',
              fraction: t.ramUsedFraction,
              value: '${formatMbAsGb(t.ramUsedMb)} / ${formatMbAsGb(t.ramTotalMb)}',
            ),
            Meter(
              label: 'Disk',
              fraction: t.diskUsedFraction,
              value: '${t.diskUsedGb.round()} / ${t.diskTotalGb.round()} GB',
            ),
          ],
        ),
      ),
    );
  }
}

class _PeerCard extends StatelessWidget {
  const _PeerCard({required this.peer});
  final Peer peer;

  @override
  Widget build(BuildContext context) {
    final state = context.read<NodeState>();
    final t = peer.telemetry;

    return Card(
      margin: const EdgeInsets.only(bottom: 10),
      child: Opacity(
        opacity: peer.online ? 1 : .55,
        child: Padding(
          padding: const EdgeInsets.all(16),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Row(
                children: [
                  Icon(deviceIcon(peer.deviceType), size: 20),
                  const SizedBox(width: 9),
                  Expanded(
                    child: Column(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        Text(peer.displayName,
                            style: const TextStyle(fontWeight: FontWeight.w600)),
                        Text(
                          '${peer.ip} · ${peer.deviceType}'
                          '${peer.latencyMs != null ? " · ${peer.latencyMs!.round()} ms" : ""}',
                          style: Theme.of(context).textTheme.bodySmall,
                        ),
                      ],
                    ),
                  ),
                  // A dot plus the word, never colour alone.
                  Row(
                    mainAxisSize: MainAxisSize.min,
                    children: [
                      Container(
                        width: 8,
                        height: 8,
                        decoration: BoxDecoration(
                          shape: BoxShape.circle,
                          color: peer.online ? BifrostColors.good : Colors.grey,
                        ),
                      ),
                      const SizedBox(width: 6),
                      Text(peer.online ? 'Online' : 'Offline',
                          style: Theme.of(context).textTheme.bodySmall),
                    ],
                  ),
                ],
              ),
              const SizedBox(height: 10),
              Meter(
                label: 'CPU',
                fraction: t.cpuUsage / 100,
                value: '${t.cpuUsage.toStringAsFixed(0)}%',
              ),
              Meter(
                label: 'RAM',
                fraction: t.ramUsedFraction,
                value: '${formatMbAsGb(t.ramAvailableMb)} free',
              ),
              const SizedBox(height: 6),
              Wrap(
                spacing: 8,
                children: [
                  TextButton.icon(
                    onPressed: peer.online
                        ? () => state.runCommand('/ping ${peer.nodeId}')
                        : null,
                    icon: const Icon(Icons.network_ping, size: 17),
                    label: const Text('Ping'),
                  ),
                  TextButton.icon(
                    onPressed: peer.online ? () => state.startCall(peer.nodeId) : null,
                    icon: const Icon(Icons.call, size: 17),
                    label: const Text('Call'),
                  ),
                  TextButton.icon(
                    onPressed: () => _rename(context, state),
                    icon: const Icon(Icons.edit_outlined, size: 17),
                    label: const Text('Rename'),
                  ),
                ],
              ),
            ],
          ),
        ),
      ),
    );
  }

  Future<void> _rename(BuildContext context, NodeState state) async {
    final controller = TextEditingController(text: peer.alias ?? '');
    final alias = await showDialog<String>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text('Name for ${peer.username}'),
        content: TextField(
          controller: controller,
          autofocus: true,
          decoration: const InputDecoration(hintText: 'Leave empty to clear'),
        ),
        actions: [
          TextButton(
              onPressed: () => Navigator.pop(context), child: const Text('Cancel')),
          FilledButton(
            onPressed: () => Navigator.pop(context, controller.text.trim()),
            child: const Text('Save'),
          ),
        ],
      ),
    );
    if (alias != null) state.setAlias(peer.nodeId, alias.isEmpty ? null : alias);
  }
}
