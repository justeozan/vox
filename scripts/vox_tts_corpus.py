#!/usr/bin/env python3
"""Benchmark corpus — sentences Vox actually says, not invented ones.

Transcribed from `conductor::build_brief_sentences` and the recap intros, plus
the announcement templates from `announce::headline`, with real project names
substituted.

Selection is deliberate, not arbitrary. Between them these cover every branch of
the sentence builder, both intro shapes, one sentence either side of the
120-character threshold that flips Kokoro's `split_pattern`, and at least one
instance of each of: accent, apostrophe, colon, em dash, '!', '?', digit,
acronym, URL, filesystem path, file name, and three project names.
"""

FR = [
    "Bonjour, voici le récap.",                                        # intro, seed%3==0
    "Salut ! Petit point sur tes worktrees.",                          # intro, mid-sentence '!'
    "L'agent claude bosse encore sur marseille.",                      # working
    "findy : l'agent codex est toujours au travail.",                  # ':' is a boundary char
    "Il y a une erreur sur vox, build cassé après le merge.",          # error + detail
    "Attention, findy est en erreur.",                                 # error, short
    "marseille attend ta réponse : est-ce que je dois merger la PR ?", # question + acronym
    "L'agent sur findy a terminé, il te faudra tester.",               # the announcement archetype
    "vox est prêt, à tester quand tu veux.",
    "marseille attend ton feu vert pour continuer.",                   # waiting
    "Rien de nouveau sur findy, vox et marseille.",                    # grouped quiet (join_names)
    # >120 chars: flips Kokoro's split_pattern branch.
    "J'ai lancé l'agent sur le refactor de l'API, il ouvre la pull request numéro 42 dès que les tests C.I. passent.",
    "Aucun worktree actif à signaler.",
    "Le fichier settings.json a changé, regarde https://github.com/justeozan/vox pour le diff.",
]

EN = [
    "Here's where things stand.",
    "Hey! Quick look at your worktrees.",
    "Agent claude is still working on marseille.",
    "findy: the codex agent is still at it.",
    "Something errored on vox, the build broke after the merge.",
    "Heads up, findy is in error.",
    "marseille is waiting on you: should I merge the PR?",
    "The agent on findy is done — you'll need to test it.",
    "vox is ready for you to try.",
    "findy is waiting on you to continue.",
    "All quiet on findy, vox and marseille.",
    "I launched the agent on the API refactor, it opens pull request 42 as soon as the CI tests pass.",
    "No active worktree to report.",
    "The file settings.json changed, see https://github.com/justeozan/vox for the diff.",
]

CORPUS = {"fr": FR, "en": EN}

if __name__ == "__main__":
    for lang, lines in CORPUS.items():
        print(f"# {lang} — {len(lines)} sentences")
        for i, t in enumerate(lines):
            print(f"  {i:2}  {len(t):3}c  {t}")
