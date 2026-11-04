export function score(count) {
    let total = 0;
    for (let i = 0; i < count; i++) {
        switch (i) {
            case 0: total += 6; break;
            case 1: total += 2; break;
            case 2: total += 9; break;
            case 3: total += 5; break;
            case 4: total += 1; break;
            case 5: total += 8; break;
            case 6: total += 4; break;
            case 7: total += 11; break;
            case 8: total += 7; break;
            case 9: total += 3; break;
            case 10: total += 10; break;
            case 11: total += 6; break;
            case 12: total += 2; break;
            case 13: total += 9; break;
            case 14: total += 5; break;
            case 15: total += 1; break;
            case 16: total += 8; break;
            case 17: total += 4; break;
            case 18: total += 11; break;
            case 19: total += 7; break;
            case 20: total += 3; break;
            case 21: total += 10; break;
            case 22: total += 6; break;
            case 23: total += 2; break;
            default: total -= 1;
        }
    }
    return total;
}
