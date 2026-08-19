// The files tab: pick a file, pick a device, watch it go.

import 'package:file_picker/file_picker.dart';
import 'package:flutter/material.dart';
import 'package:open_filex/open_filex.dart';
import 'package:provider/provider.dart';

import '../main.dart';
import '../models/models.dart';
import '../state/node_state.dart';

class FilesScreen extends StatefulWidget {
  const FilesScreen({super.key});

  @override
  State<FilesScreen> createState() => _FilesScreenState();
}

class _FilesScreenState extends State<FilesScreen> {
  String? _target;

  Future<void> _pickAndSend() async {
    final state = context.read<NodeState>();
    final target = _target;
    if (target == null) {
      ScaffoldMessenger.of(context).showSnackBar(
        const SnackBar(content: Text('Choose a device to send to first')),
      );
      return;
    }

    final picked = await FilePicker.platform.pickFiles(allowMultiple: true);
    if (picked == null) return;

    for (final file in picked.files) {
      final path = file.path;
      if (path != null) state.sendFile(target, path);
    }
  }

  @override
  Widget build(BuildContext context) {
    final state = context.watch<NodeState>();
    final online = state.onlinePeers;

    // A device that goes offline must not stay selected.
    if (_target != null && !online.any((p) => p.nodeId == _target)) {
      _target = null;
    }

    return ListView(
      padding: const EdgeInsets.fromLTRB(14, 14, 14, 28),
      children: [
        Card(
          child: Padding(
            padding: const EdgeInsets.all(16),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text('Send a file', style: Theme.of(context).textTheme.titleSmall),
                const SizedBox(height: 12),
                DropdownButtonFormField<String>(
                  value: _target,
                  decoration: const InputDecoration(
                    labelText: 'To',
                    border: OutlineInputBorder(),
                    isDense: true,
                  ),
                  items: online
                      .map((p) => DropdownMenuItem(
                          value: p.nodeId, child: Text(p.displayName)))
                      .toList(),
                  onChanged: (value) => setState(() => _target = value),
                ),
                const SizedBox(height: 12),
                SizedBox(
                  width: double.infinity,
                  child: FilledButton.icon(
                    onPressed: online.isEmpty ? null : _pickAndSend,
                    icon: const Icon(Icons.upload_file),
                    label: Text(online.isEmpty ? 'No devices online' : 'Choose a file'),
                  ),
                ),
              ],
            ),
          ),
        ),
        const SizedBox(height: 14),
        Padding(
          padding: const EdgeInsets.only(left: 4, bottom: 8),
          child: Text('Transfers', style: Theme.of(context).textTheme.titleSmall),
        ),
        if (state.transfers.isEmpty)
          const Card(
            child: Padding(
              padding: EdgeInsets.all(22),
              child: Text('No transfers yet.', textAlign: TextAlign.center),
            ),
          )
        else
          ...state.transfers.take(25).map((t) => _TransferTile(transfer: t)),
      ],
    );
  }
}

class _TransferTile extends StatelessWidget {
  const _TransferTile({required this.transfer});
  final FileTransfer transfer;

  @override
  Widget build(BuildContext context) {
    final colour = transfer.state.isFailed
        ? BifrostColors.critical
        : transfer.state.isDone
            ? BifrostColors.good
            : Theme.of(context).colorScheme.primary;

    final subtitle = [
      transfer.peerName,
      '${formatBytes(transfer.bytesDone)} of ${formatBytes(transfer.bytesTotal)}',
      if (transfer.state.isActive && transfer.rateBps > 0)
        '${formatBytes(transfer.rateBps)}/s',
      if (transfer.state.detail != null) transfer.state.detail!,
    ].join(' · ');

    return Card(
      margin: const EdgeInsets.only(bottom: 8),
      child: Padding(
        padding: const EdgeInsets.all(14),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                Icon(
                  transfer.incoming ? Icons.south : Icons.north,
                  size: 17,
                  color: colour,
                ),
                const SizedBox(width: 8),
                Expanded(
                  child: Text(transfer.filename,
                      overflow: TextOverflow.ellipsis,
                      style: const TextStyle(fontWeight: FontWeight.w600)),
                ),
                // Label as well as colour, so state never rests on hue alone.
                Text(transfer.state.label,
                    style: Theme.of(context).textTheme.bodySmall),
              ],
            ),
            const SizedBox(height: 4),
            Text(subtitle, style: Theme.of(context).textTheme.bodySmall),
            const SizedBox(height: 8),
            ClipRRect(
              borderRadius: BorderRadius.circular(4),
              child: LinearProgressIndicator(
                value: transfer.progress,
                minHeight: 6,
                backgroundColor: colour.withOpacity(.18),
                valueColor: AlwaysStoppedAnimation(colour),
              ),
            ),
            if (transfer.state.isDone && transfer.savedPath != null) ...[
              const SizedBox(height: 6),
              Align(
                alignment: Alignment.centerLeft,
                child: TextButton.icon(
                  onPressed: () => OpenFilex.open(transfer.savedPath!),
                  icon: const Icon(Icons.open_in_new, size: 17),
                  label: const Text('Open'),
                ),
              ),
            ],
          ],
        ),
      ),
    );
  }
}
