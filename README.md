# Serein · fork Its-Ora

Fork perso de [Serein](https://github.com/ViceVerse-cz/Serein), client Discord natif en Rust.
Ce README ne liste que ce qui change par rapport au client de base.

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

**Divers**
- Mise à jour auto désactivée (elle réinstallerait la version officielle).

## Licence

Code d'origine sous MIT ou Apache-2.0, voir [LICENSE-MIT](LICENSE-MIT) et [LICENSE-APACHE](LICENSE-APACHE).
