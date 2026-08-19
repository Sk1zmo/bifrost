// The chat tab. Broadcast by default; pick a device to send privately.

import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../models/models.dart';
import '../state/node_state.dart';

class ChatScreen extends StatefulWidget {
  const ChatScreen({super.key});

  @override
  State<ChatScreen> createState() => _ChatScreenState();
}

class _ChatScreenState extends State<ChatScreen> {
  final _input = TextEditingController();
  final _scroll = ScrollController();
  String? _recipient;
  int _lastCount = 0;

  @override
  void dispose() {
    _input.dispose();
    _scroll.dispose();
    super.dispose();
  }

  void _send() {
    final text = _input.text.trim();
    if (text.isEmpty) return;
    final state = context.read<NodeState>();

    // A slash command is a command wherever it is typed, so `/status` on a
    // phone answers exactly as it does in a terminal.
    if (text.startsWith('/')) {
      state.runCommand(text);
    } else if (_recipient != null) {
      state.directMessage(_recipient!, text);
    } else {
      state.broadcast(text);
    }
    _input.clear();
  }

  /// Scroll to the newest message, but only if the reader was already there.
  void _followIfAtBottom(int count) {
    if (count == _lastCount) return;
    _lastCount = count;

    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (!_scroll.hasClients) return;
      final position = _scroll.position;
      if (position.maxScrollExtent - position.pixels < 200) {
        _scroll.jumpTo(position.maxScrollExtent);
      }
    });
  }

  @override
  Widget build(BuildContext context) {
    final state = context.watch<NodeState>();
    final messages = state.chats;
    final me = state.snapshot?.nodeId ?? '';
    _followIfAtBottom(messages.length);

    return Column(
      children: [
        _RecipientBar(
          value: _recipient,
          peers: state.onlinePeers,
          onChanged: (value) => setState(() => _recipient = value),
        ),
        Expanded(
          child: messages.isEmpty
              ? const Center(child: Text('No messages yet. Say hello.'))
              : ListView.builder(
                  controller: _scroll,
                  padding: const EdgeInsets.fromLTRB(12, 8, 12, 8),
                  itemCount: messages.length,
                  itemBuilder: (context, index) =>
                      _Bubble(message: messages[index], mine: messages[index].senderId == me),
                ),
        ),
        SafeArea(
          top: false,
          child: Padding(
            padding: const EdgeInsets.fromLTRB(12, 6, 12, 10),
            child: Row(
              children: [
                Expanded(
                  child: TextField(
                    controller: _input,
                    textInputAction: TextInputAction.send,
                    onSubmitted: (_) => _send(),
                    decoration: InputDecoration(
                      hintText: _recipient == null
                          ? 'Message everyone, or /help'
                          : 'Private message',
                      border: OutlineInputBorder(
                        borderRadius: BorderRadius.circular(22),
                      ),
                      contentPadding:
                          const EdgeInsets.symmetric(horizontal: 16, vertical: 10),
                    ),
                  ),
                ),
                const SizedBox(width: 8),
                IconButton.filled(onPressed: _send, icon: const Icon(Icons.send)),
              ],
            ),
          ),
        ),
      ],
    );
  }
}

class _RecipientBar extends StatelessWidget {
  const _RecipientBar({
    required this.value,
    required this.peers,
    required this.onChanged,
  });

  final String? value;
  final List<Peer> peers;
  final ValueChanged<String?> onChanged;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.fromLTRB(14, 10, 14, 4),
      child: Row(
        children: [
          Text('To', style: Theme.of(context).textTheme.bodySmall),
          const SizedBox(width: 10),
          Expanded(
            child: DropdownButton<String?>(
              value: peers.any((p) => p.nodeId == value) ? value : null,
              isExpanded: true,
              underline: const SizedBox.shrink(),
              items: [
                const DropdownMenuItem(value: null, child: Text('Everyone')),
                ...peers.map((p) =>
                    DropdownMenuItem(value: p.nodeId, child: Text(p.displayName))),
              ],
              onChanged: onChanged,
            ),
          ),
        ],
      ),
    );
  }
}

class _Bubble extends StatelessWidget {
  const _Bubble({required this.message, required this.mine});
  final ChatMessage message;
  final bool mine;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final time =
        '${message.timestamp.hour.toString().padLeft(2, "0")}:${message.timestamp.minute.toString().padLeft(2, "0")}';

    return Align(
      alignment: mine ? Alignment.centerRight : Alignment.centerLeft,
      child: Container(
        constraints: BoxConstraints(
            maxWidth: MediaQuery.of(context).size.width * .78),
        margin: const EdgeInsets.symmetric(vertical: 4),
        child: Column(
          crossAxisAlignment:
              mine ? CrossAxisAlignment.end : CrossAxisAlignment.start,
          children: [
            Padding(
              padding: const EdgeInsets.symmetric(horizontal: 4, vertical: 2),
              child: Text(
                '${mine ? "You" : message.senderName}'
                '${message.isDirect ? " · private" : ""} · $time',
                style: Theme.of(context).textTheme.bodySmall,
              ),
            ),
            Container(
              padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 9),
              decoration: BoxDecoration(
                color: mine ? scheme.primary : scheme.surfaceContainerHighest,
                borderRadius: BorderRadius.circular(16),
                border: message.isDirect
                    ? Border.all(color: scheme.outline, style: BorderStyle.solid)
                    : null,
              ),
              child: Text(
                message.content,
                style: TextStyle(color: mine ? scheme.onPrimary : null),
              ),
            ),
          ],
        ),
      ),
    );
  }
}
