# Serein · fork Its-Ora

Fork perso de [Serein](https://github.com/ViceVerse-cz/Serein), client Discord natif en Rust.
Ce README ne liste que ce qui change par rapport au client de base. Le fork suit les mises à jour officielles (dernière fusion : novembre 2026).

## Télécharger

Onglet **[Releases](https://github.com/Its-Ora/Serein/releases/latest)** → `serein-ora-setup.exe` (installeur) ou `serein-ora-windows-portable.zip`.
Windows uniquement. Chaque push sur `main` publie une nouvelle release automatiquement.

## Changements

**Barre des serveurs**
- Bouton **Tout lire** sous le logo : marque tous les serveurs comme lus en un clic (les MP ne sont pas touchés).
- Plus de pastille blanche de non-lu à gauche des serveurs.
- Pings en couleur : **bleu** = @ direct, **rouge** = rôle / @everyone / @here, les deux côte à côte si besoin.

**Amis**
- Groupes d'amis (bouton « + Nouveau groupe », menu ⋯ d'un ami → Déplacer vers le groupe). Stockés sur le PC.

**Bloqués et ignorés**
- Invisibles partout : messages, liste des membres, pings, « est en train d'écrire ».
- En vocal : « Une personne bloquée dans ce vocal », anonyme, mutée d'office, impossible à démuter.

**Statut**
- **En ligne · silencieux** : visible en ligne, mais aucune notif, aucun son, aucun badge.

**Connexion**
- Plus de déconnexion toutes les ~18 min : Serein garde l'identité exacte de la fenêtre de login (le décalage d'identité faisait révoquer la session par Discord).
- Un 401 ne vide plus le token : vérification de `/users/@me`, puis reconnexion auto (30 s, 2 min, 5 min).
- Journal : `%LOCALAPPDATA%\serein\session.log` (aucun token ni ID dedans).

**Streams (regarder)**
- Plusieurs streams en même temps (4 max), côte à côte comme sur Discord. Le son des streams est mixé.
- Option **Masquer les gens sans stream** (bouton volume du stream ou clic droit sur un stream).
- 1440p, 4K et ultrawide s'affichent (avant : son OK mais image bloquée sur « waiting for video »).
- Bascule auto vers le décodage logiciel si le décodeur GPU ne sort aucune image.

**Streams (partager)**
- Débit toujours au maximum accepté par Discord (≈ 10 Mb/s), quelle que soit la résolution ; il ne baisse que si le réseau sature. Avant : 2 à 8 Mb/s selon la résolution.
- Résolutions : 480p, 720p, **936p** (1664×936, le meilleur compromis à 10 Mb/s), 1080p.
- Fréquences : 15, **23,976 (film)**, 30, 60, 90, 120 fps (90 et 120 demandent un écran assez rapide).

**Vocal**
- Quand un modo te déplace dans un autre salon (même un salon où tu n'as pas le droit d'aller), Serein te suit sans couper le son : la connexion vocale est gardée, seul le salon change. Si Discord change de serveur vocal, Serein se reconnecte avec la même session. Avant, l'appel devenait muet et le bouton « Rejoindre » restait affiché.
- Plus de vocal bloqué : si un appel échoue alors que Discord t'a déjà mis dans un salon, Serein te fait vraiment quitter ce salon (avant, il oubliait l'appel de son côté seulement : tu entendais les autres, sans pouvoir parler, quitter ni rejoindre un autre vocal). Rejoindre un autre salon pendant qu'un vieil appel traîne bascule directement dessus au lieu d'être refusé.

**Stickers**
- Un sticker d'un autre serveur (sans Nitro) part direct en image, sans passer par la barre de message.

**Divers**
- Mise à jour auto désactivée (elle réinstallerait la version officielle).

## Licence

Code d'origine sous MIT ou Apache-2.0, voir [LICENSE-MIT](LICENSE-MIT) et [LICENSE-APACHE](LICENSE-APACHE).
